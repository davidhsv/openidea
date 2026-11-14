# GPUI port — decisions

Every non-obvious call made while porting the webview UI to GPUI, with the reason. "Measured" means the command was run in the port's worktree and its output is what is quoted.

## D1. `app-gpui/` is its own Cargo workspace

The GPUI app lives in `app-gpui/` and is listed under `exclude` in the root `Cargo.toml`. The root workspace, its `Cargo.lock` and its clippy / deny / nextest runs stay exactly what the Tauri app is built from, so the final comparison builds the baseline from an unchanged tree. The cost is a second lock file; D9 keeps the shared backend dependencies on the same versions.

## D2. GPUI comes from crates.io as `gpui-pre =0.3.12`, a snapshot of zed@d2a4fb6

- Pinned Zed commit: `d2a4fb6ded7c470be25d1541281da3882495b65b` (committed 2026-11-08).
- The official `gpui` crate on crates.io is 0.2.2 from 2025-10-22, more than a year behind Zed's `main`. `gpui-pre` republishes `crates/gpui` weekly under Apache-2.0.
- Provenance, measured: a shallow fetch of zed at that commit and `diff -rq crates/gpui/src <registry>/gpui-pre-0.3.12/src` reports 1 differing file of 96, `action.rs`, where the `actions!` macro derives `$crate::Action` instead of `gpui::Action` (needed because the package is renamed).
- A git dependency on zed itself was the alternative. It was not taken because `gpui-component` (D3) depends on `gpui-pre` by name and Cargo cannot patch a package with one of a different name; two GPUI copies in one binary do not interoperate.

## D3. Widgets come from `gpui-kit` / `gpui-component` 0.9.1 (Apache-2.0)

The webview UI's `src/ui` primitives (inputs with IME, lists, trees, virtual lists, menus, popovers, tooltips, tabs, splitters, scrollbars, dialogs) each need a GPUI counterpart. `gpui-component` provides them; writing them again would add months and nothing the user sees. Its look is replaced by the IDE's own tokens (D6). It is used for primitives only: the dock/tool-window model, the editor and the terminal are the port's own code, because their behaviour is specified by this repository's tests.

`gpui-component`'s `tree-sitter` feature is off: it links tree-sitter 0.26 while the backend links 0.25 (`links = "tree-sitter"` allows one). Highlighting comes from `ide-syntax`, as in the Tauri app.

## D4. No Zed crate other than `gpui`

Measured at the pinned commit: `crates/editor`, `crates/terminal` and `crates/ui` are `GPL-3.0-or-later`; only `crates/gpui` is Apache-2.0. This repository is Apache-2.0, so the editor and terminal views are written here.

## D5. The app calls `ide_host::ops` directly

`src-tauri/src/commands/*.rs` are thin adapters over `ide_host::ops::<domain>` (the same functions `openidea-server` dispatches for remote development) plus desktop-shell commands. The GPUI app links `ide-host` and calls those functions with typed arguments: no JSON, no IPC. `ide_core::EventSink` is implemented by a channel drained on the GPUI main thread. Desktop-shell commands (windows, menus, updates) are reimplemented against GPUI; where a shell command holds logic both apps need, it moves into a backend crate rather than being copied.

## D6. Design data is read from the webview sources, not retyped

`src/styles/tokens.css`, the keymap presets, the text catalogs, the icons and the static metadata of every action and contribution (id, title, category, `when` clause, icon, anchor, order…) are the source of truth while both apps exist. `tokens.css`, the locale JSON files and the SVG files are embedded as they are. What lives in TypeScript is extracted by `docs/gpui-port/gen-data.mjs` (it evaluates the keymap modules and resolves object literals through the TypeScript program) into `app-gpui/assets/gen/*.json`, which is checked in; `gen-data.mjs --check` fails when a file is stale. Correction (2026-11-13): no Rust test fails for an action in `actions.json` without a handler; an earlier version of this entry said one did. Which actions have a handler is tracked by the Actions section of `INVENTORY.md`, whose rows need a named test. The generator also drops, without a warning, a `when` / `keyWhen` that an action gets through a helper, a call or a template string, actions whose `run` is added later in a `.map`, and ids built at run time (audit: 16 actions with a default shortcut lost their condition, 50 are absent; none registered natively is unguarded today, see D80).

## D7. Terminal emulation: `alacritty_terminal` 0.26 (Apache-2.0)

`ide-terminal` owns the PTY and streams bytes; the webview parses them with xterm.js. The GPUI app feeds the same bytes to `alacritty_terminal`'s grid and paints the grid.

Status: a terminal view exists and runs a real shell (D27). It is not in a tool window yet.

## D8. HTML surfaces

| Surface                                                                                                            | GPUI app                                                                                    |
| ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------- |
| Markdown (preview, agent chat, GitHub, LSP hover)                                                                  | Rendered natively                                                                           |
| Plugin web views (`openidea-plugin:` scheme), the Web editor's browser bar, notebook HTML output, Mermaid diagrams | A WKWebView created through `wry` when the surface is first shown, destroyed when it closes |

Plugin web views are HTML by contract (plugin API 1.4), so a web view cannot be removed without breaking plugins. It is created on demand, so a session that opens none of these surfaces has no WebKit process; the final measurement reports both cases.

Status: the web view works inside a GPUI window and the plugin host runs in the GPUI app (D26). `wry` is pinned to 0.57.0, the version the Tauri build's lock holds. No plugin web view, browser bar, notebook output or Mermaid diagram is shown through it yet.

## D9. Shared dependencies resolve to the root lock's versions

`app-gpui/Cargo.lock` was seeded from the root `Cargo.lock`, so Cargo only added what GPUI needs. `node docs/gpui-port/lockcheck.mjs` compares the dependency closure of `ide-host` in both locks and fails on any package the two builds use at different versions. Measured after seeding: 12 packages differ and are allowlisted in the script, all forced by `gpui-kit`'s optional `gpui-fast-web` (never compiled here, but part of lock resolution): the ICU 2.3 → 2.4 family (`icu_collections`, `icu_locale_core`, `icu_normalizer`, `icu_normalizer_data`, `icu_properties`, `icu_properties_data`, `icu_provider`, `tinystr`, `yoke`, `zerovec`, `zerovec-derive`) and `unicode-properties` 0.1.4 → 0.1.3. Before seeding, a fresh resolution had moved roughly a hundred backend dependencies to newer patch releases.

## D10. Commits go through the repository's pre-commit hook with pnpm 10

The hook runs `pnpm lint-staged`; the `pnpm` on `PATH` here is 9.15, which rejects this repository's `pnpm-workspace.yaml`. Commits run with a shim that executes `corepack pnpm@10.34.9` (the version that installed the main checkout's `node_modules`) rather than skipping the hook.

## D11. Per-window state replaces per-webview singletons

Each Tauri window is its own webview, so the webview UI keeps context keys, tabs, tool-window layout and the open project in module-level stores. The GPUI app is one process with many windows: that state lives in one `Workbench` entity per window, and only what really is process-wide (the action registry's definitions, the keymap, settings, the backend `Host`) is a GPUI global. An action's handler receives the window it was invoked in.

## D12. The keymap dispatcher is the port's own, not GPUI's

GPUI has a key-binding system, but this IDE's binding syntax (two-stroke chords with a 2 s timeout, double-modifier gestures such as `Shift Shift` and `Alt Alt+Up`, mouse-button and swipe bindings, `when` / `keyWhen` clauses over context keys, plugin bindings that yield to the preset) is specified by `src/platform/keymap.test.ts`. The dispatcher is ported as a state machine fed from GPUI's keystroke, modifier and mouse events at the window root, so the same tests apply.

## D13. The GPUI app has its own directories

The Tauri app keeps settings, recent projects, credentials and caches under the identifier `ai.l0ng.openidea`. The GPUI app uses `ai.l0ng.openidea.gpui` (`.gpui.dev` in debug builds), so it can run next to an installed Tauri build without the two processes writing the same files, and `OPENIDEA_GPUI_HOME` puts every directory under one folder (tests, and the final measurement, which starts both apps from empty directories). The cost: the GPUI app does not pick up the Tauri app's existing settings; importing them is a copy of the configuration folder.

## D14. The editor shows the backend's rope; edits are synchronous calls

The webview keeps its own copy of every open document in CodeMirror and reconciles it with the backend's through an OT client (`src/editor/core/sync/DocSync.ts`), because edits cross an IPC boundary. In process there is no boundary: the editor holds a clone of the backend's rope at a known version (a `ropey` clone shares its chunks, so the text exists once) and applies each edit with `Documents::edit(doc, version, changes)` before repainting. Edits the backend makes itself arrive as `DocPatched` and move the selection; an edit rejected as out of sync (a backend edit not delivered yet) refreshes the text and is re-made at the caret's new place. `Documents::rope_snapshot` is the one backend API added for this (`crates/ide-workspace/src/documents.rs`, additive).

Undo stays where the webview has it, on the UI side: the editor records each edit with its inverse and the selections around it (`editor/history.rs`, grouped like CodeMirror's history: continuous typing within 500 ms is one step, a caret move starts another). An edit the backend makes is a step too, as it is in the webview, except a workspace edit, which is undone as a whole elsewhere and resets the editor's history.

Open risk: `Documents::edit` takes the store's operation lock on the main thread. A long backend operation holding that lock (save all, sync from disk) would stall typing; if a measurement shows it, edits move to a queue with the rebase logic `DocSync` has.

## D15. Highlighting comes from tree-sitter, not Lezer

The webview highlights most languages with CodeMirror's Lezer grammars, which do not exist outside a browser; only languages without a CodeMirror mode use the backend's tree-sitter queries (`doc_highlights`). The GPUI editor uses `doc_highlights` for every language and maps its kinds to the `--syn-*` tokens exactly as the webview's highlight style does.

For the languages CodeMirror covers, `ide-language` gains a cargo feature, `native-highlights`, that the GPUI app enables: it supplies each grammar crate's own `highlights.scm` (26 grammars) through the same code path. The Tauri build leaves the feature off, so its binary and behaviour are unchanged (checked: clippy and the highlight tests pass in both configurations). Those upstream queries capture declared and called functions alike; for Rust, JavaScript/TypeScript/TSX, Python, Go, Java, C and C++ a few extra patterns mark declarations, because the colour scheme distinguishes them. The other languages colour declarations like calls for now. Protocol Buffers has no query in the locked grammar version. Token boundaries will differ from Lezer's in places; that is a deviation this port cannot avoid.

## D16. Keys are named by the layout, not by the physical key

The webview names a key from `KeyboardEvent.code` (physical position) with a fallback to the typed letter, so that ⌥-letter works on macOS and AZERTY's Ctrl+Z is not Ctrl+W. GPUI reports the layout-resolved key (the character the key types without modifiers, or its ASCII equivalent on non-Latin layouts), which gives the same answers for both cases; it has no physical code, so numeric-keypad operators are not told apart from the main-row ones. Bindings that name `NumpadAdd` / `NumpadSubtract` / `NumpadMultiply` / `NumpadDivide` therefore fire from neither key until a platform hook supplies the distinction.

## D17. Fonts: not embedded yet

The webview bundles Inter Variable and JetBrains Mono Variable as WOFF2 subsets per Unicode range (`@fontsource-variable/*`). GPUI's text system loads TTF/OTF data, and several same-named subset fonts do not combine into one family there. The editor currently uses the first installed family of `--font-mono` (falling back to Menlo) and the UI uses the system font. Embedding the two families as full TTFs is open.

## D18. Pure-logic ports are checked against the original's own engine where one exists

Number formats and plural categories are rules the webview gets from `Intl`. `gen-data.mjs` writes what Node's `Intl.NumberFormat` and `Intl.PluralRules` answer for the seven UI locales to `assets/gen/intl.json`, and a Rust test compares every row. It caught two differences on its first run (negative zero keeps its sign; Spanish treats -1 as `one`).

## D19. Caret movement and typing are the editor's own keys, not actions

The manifest has no actions for arrows, Home/End, Backspace or Enter: in the webview they are CodeMirror's base keymap (`src/editor/core/cm/baseKeymap.ts`), below the IDE keymap. The GPUI editor handles them in its own key handler for the same reason, after the dispatcher has had the keystroke. Undo, redo, cut, copy, paste and select-all are both: registered actions (`edit.*`, what the Edit menu runs) and the editor's ⌘-keys, since the webview gets those shortcuts from the native menu's accelerators and the GPUI app has no native menu yet.

## D20. What has been seen on screen

A window-only capture of the running debug build (2026-11-11) shows the editor drawing a Zig file with gutter numbers, caret, current-line band and syntax colours in Islands Light. Nothing else has been looked at: the window tests run on GPUI's test text system, whose glyph metrics are not the real shaper's, so pixel positions asserted there prove the mapping logic, not the rendering.

## D21. `localStorage` becomes one JSON file

The webview keeps session-outliving UI state (tool window layouts, named layouts, search history, dialog choices) in `localStorage` under string keys. The GPUI app keeps the same keys with the same JSON values in `ui-storage.json` in its configuration directory, written through on every change by rename, so a layout stored by one app parses in the other. The two apps do not share the file (D13).

## D22. A tool window appears on a stripe only once it has a view here

The webview registers 36 tool windows. The GPUI stripes show a button only for those this app can draw (today: Project), so no button opens an empty pane. Their declarations (title, icon, side, order) come from the contributions manifest, and each gets its `toolwindow.<id>` action when its view is ported.

## D23. Unsaved text is never dropped

Measured in the webview's sources: when a text document's last tab closes, `DocumentManager.release` saves it if `files.autoSave` is on (the default) and then disposes it whether or not the save worked; with `files.autoSave` off it is disposed unsaved. Switching tabs and the window losing focus also autosave. Closing a window with unsaved documents asks "Unsaved Changes" with Save All / Don't Save (`closeGuard.ts`).

The GPUI app saves on the same three occasions and asks the same question on window close. It differs in two places, both on the side of keeping the text: a tab whose save fails reopens with its editor and an error notification instead of being disposed, and with `files.autoSave` off closing an unsaved tab asks the same "Unsaved Changes" question (Save All / Don't Save / Cancel, Cancel bringing the tab back) where the webview discards the edits without asking. With autosave off nothing is saved on leaving a tab or the window, as in the webview.

The editor marks itself unsaved on its own successful edit rather than waiting for the backend's `DocDirty` event, so typing and closing in the same instant cannot slip between the two.

## D24. One collation, on ICU, checked against `Intl.Collator`

The webview sorts every list with `localeCompare` or `Intl.Collator`, in four option sets: default, `sensitivity: "base"`, `numeric: true`, and both. `platform::collate` gives the same four on `icu_collator` 2.4.0, the ICU version GPUI already locks (no new version in the lock; `lockcheck.mjs` passes). It is one new direct dependency, justified because code-point ordering visibly differs from the webview's for case, accents and punctuation (`_build` before `1`, `main` before `Main`, `résumé` next to `resume`).

`gen-data.mjs` writes `assets/gen/collation.json`: 75 awkward names sorted by Node's `Intl.Collator("en", …)` under each option set. It also holds the sign of every pair of those names under each option set (22,500 comparisons), so names the collator holds equal are told from names that were merely listed in order. `platform::collate::tests::orders_names_as_intl_collator_does` requires the same order and the same signs from the Rust side. The webview passes `undefined` as the locale, so it follows the system's; the GPUI app uses the root collation, which is what English and most European locales resolve to. Locale tailorings (Swedish `ä` after `z`, for example) are not applied.

The six hand-written orderings this replaces (tool windows, tabs, main toolbar, project tree, themes and fonts, version control lists) are gone.

## D25. One settings service; the theme is per process

`platform::settings` is the only way the UI reads and writes settings: it calls the backend's store directly and then announces `SettingsChanged` to the windows the value concerns, the writing window included (the Tauri glue did the announcing; here `Backend::emit` puts UI-side events on the same ordered queue as the backend's). What depends on a setting listens for that event, so it is refreshed the same way whoever wrote the value. The four ported copies of `SettingsWrite` and the theme module's own variant are this module's one type now. A write runs on the main thread: it is one small TOML file, and the work a value may trigger (language servers, file types) is already spawned by the backend.

Not carried over from the Tauri glue, because their features are not ported yet: re-applying the MCP server on `mcpServer.*` and waking the update checker on a global write.

`theme::appearance` applies the `appearance` settings: the chosen theme, or with Sync with OS the preferred light/dark theme for the OS mode (GPUI's window-appearance observer), a custom accent, and a session override when the choice cannot be saved. It reads the **global** settings and sets one theme for the process, because the theme is a process global here (D11). The webview applies each window's effective settings, and a project's settings layer may hold `appearance.*` (measured: `remove_global_settings` in `crates/ide-workspace/src/settings.rs` strips only `mcpServer` and `editor.customSchemes` from it), so there a project can give its window its own theme; the GPUI app ignores such a project-level override. That is a parity gap, listed in the inventory under `src/platform/themes.test.ts`. Plugin themes resolve to their light/dark base until the plugin host is connected.

Seen on screen (D20): the first frame of a window with `appearance.theme = "dark"` is dark. Nothing after the first frame could be captured in that session: the display was asleep and the screen locked (measured with `CGDisplayIsAsleep`), and GPUI draws further frames from the display link.

## D26. The web view and the plugin host are proven in the GPUI app

**Web view (confirms D8).** GPUI's `Window` implements `HasWindowHandle`, and `wry` builds a WKWebView as a child of it (`web_view::WebSurface`). GPUI's test windows have no native view, so this is checked in the real application: `openidea-gpui --self-check=web-view` opens a window, loads a page, receives `window.ipc.postMessage` from it, runs script in it, resizes the surface and waits for the page to report the new size. The integration test `tests/web_view.rs` starts the binary and requires `web-view: ok`. It passed with the display asleep and the screen locked, so it does not depend on frames being drawn. `inventory.mjs` accepts such tests as proofs, cited as `tests::<file>::<case>`.

**Plugin host (no extraction needed).** Measured: the plugin host is `crates/ide-host/src/plugins/` (about 20,800 lines); `src-tauri/src/commands/plugin.rs` (1,067 lines) only creates it and forwards commands. `plugins::Plugins::init` in the GPUI app does the same creation (data directory, language store, external syntax and run detection, workspace, trust policy, secret store, the load/flush/supervise thread) and brings `PluginEvent`s to the main thread. Nothing under `src-tauri` was changed. Left out because the features are not ported: syncing plugins to remote-development servers.

Plugin secrets go to the login keychain under this app's own identifier (`<APP_IDENTIFIER>.plugin.<id>`), not the installed app's `ai.l0ng.openidea.plugin.<id>`: like the data directories (D13), a development build does not read or overwrite the installed app's entries. At switchover the service name is the one line to change.

First contribution projected: the `themes` point. A plugin's theme is offered next to the built-in ones, the host's selected theme follows `appearance.theme`, and its tokens are applied over the built-in base of its kind, under a custom accent. `plugins::tests::a_linked_plugins_theme_is_offered_and_applied` does this with `plugins/official/harbor-night`. Project plugins (`<project>/.openidea/plugins`) are not listed yet: the listing is per process, not per window.

**The Bazel plugin.** `/Users/davidvieira/code/openidea-bazel` was linked into a temporary home by the opt-in test `a_plugin_folder_named_by_the_environment_loads` (run with `OPENIDEA_GPUI_TEST_PLUGIN`). Measured result: `davidhsv.bazel 0.1.0: Inactive` (contributions live, no Wasm instance yet), 9 commands, 6 menus, 1 settings contribution, and the languages `starlark`, `bazelproject`, `bazelrc` registered. With `OPENIDEA_GPUI_TEST_COMMAND=open-sync-log` the same test runs that command without a project: the plugin's Wasm loaded (status `Active`) and answered `no project is open`, its own message. The installed app's folders were not touched.

**Commands.** Every declared command of a live plugin is an action `plugin.<plugin id>.<command id>` with its title, category and `when`, registered before any of its code is loaded. Running one passes the window's project, its active editor (path, language, selection as lines and UTF-16 columns) and the selected paths; a failure is a notification in that window unless the plugin was stopped or lacks a permission. `plugins::tests::a_plugins_command_runs_its_wasm_on_the_editors_selection` does this with `plugins/official/string-tools`: the command turns the selected `hello big world` into `hello_big_world` through the plugin's Wasm, the backend's document and the editor's patch path. A command's own image icon is not loaded yet (a named icon is used, otherwise the package icon).

## D27. The terminal: emulator, keys and view

Three parts, each tested on its own:

- `terminal::emulator` wraps `alacritty_terminal`'s `Term`: output in, rows of styled runs out (a wide character is its own two-cell run, so text stays on the grid), cursor, scrollback of 10,000 lines as in the webview, selection by cell, word and line, and the answers a program is owed (device reports, colour queries, text-area size) as effects to write to the PTY. A program may set the clipboard (OSC 52) but never read it.
- `terminal::encode` is what xterm.js did implicitly: xterm's key sequences (cursor keys in normal and application mode, modifiers, function keys, control characters, Option as Meta from `terminal.optionAsMeta`, Option+Left/Right as word movement) and paste encoding (Enter line ends, bracketed-paste markers, a marker inside the pasted text removed so it cannot end the bracket early). Bold text is not brightened, as the webview sets `drawBoldTextInBrightColors: false`.
- `terminal::view::TerminalView` spawns the shell through `ide_host::ops::term`, feeds its output to the emulator, sizes the grid to the room it is painted in and tells the PTY, and reports the exit with the webview's `[Process completed (exit code N)]` line.

`terminal::view::tests::a_shell_runs_in_the_terminal_and_reports_its_exit` runs the login shell on a real PTY: typed text and Enter reach it, its output is on the grid, `stty size` prints the fitted grid's size, ⌘V pastes, and `exit 3` is reported as exit code 3.

Not done: the Terminal tool window with its tabs and groups, restart on Enter after exit, the find bar, links, mouse reporting to programs, the resize debounce, cursor blink, the Nerd-font fallbacks, and the derived palette for themes that define neither `--terminal-ansi-*` nor `--ansi-*` (every built-in theme defines them). The cell is the font's `M` advance by its ascent plus descent times `terminal.lineHeight`; xterm.js measures its cells its own way, so the grid may differ by a column or a row from the webview's at the same pixel size.

## D28. Tool windows dock on three sides; a hidden one keeps its view

The workbench lays tool windows out as the webview's grid does: the editor between the left and right sides, the bottom side spanning below all three, each side with a primary and a split area sharing it by the stored ratio. The left stripe lists Left, Left Bottom and at its foot Bottom Left; the right stripe (shown only when it has a window) lists Right, Right Bottom and Bottom Right. Supersedes D22's "today: Project": the Terminal is the second window.

A window's view is created the first time it is shown and kept while hidden, as the webview keeps mounted windows: hiding the Terminal does not end its shell. The Terminal starts as Settings › Terminal says (start directory, shell command line, environment, shell integration).

Not carried over yet: resize handles, maximize, the widescreen layout (side windows at full height), tool window names under the icons, drag and drop on the stripes, header actions and menus.

## D29. The editor session: same stored JSON, tabs loaded when shown

`editor::session` stores a project's tabs, splits, active pane and per-file caret and scroll position under the webview's key (`openidea.editorSession.v1:<project folder>`) in the webview's JSON shape, validated on the way in. It is written 400 ms after the tabs last changed, when the window loses focus and when it goes away.

A restored tab is only a title until it is shown: its document is opened in the backend when the tab becomes the active one, and its saved selection and first visible line are applied then. The webview does the same (`ensureSlot` on the active tab of each pane). Opening a tab's document is idempotent per tab: the first version of this change opened it twice, and the second arrival took the focus back from wherever the user had gone (caught as a 1-in-4 test failure).

Only one pane is drawn so far: a saved split layout is restored into the model and kept on save, but its other panes are not visible. Tabs of non-file editor kinds are not saved, because no such kind exists here yet.

## D30. The Git log's first slice, and what the session restore must not do

The Git tool window (bottom, as declared) holds `vcs::log_view::GitLog`: commits read 300 at a time through `ide_host::ops::vcs::vcs_log` as the list nears its end, the graph from the ported `vcs::graph::compute_graph` drawn as the webview draws it (14 px lanes, 26 px rows, straight edges, dashed for commits left out, a ring for HEAD), reference labels by the webview's rules (`vcs::log_options::ref_labels`), author and date in the user's date format and local time. It is read again on the backend's `VcsChanged`, keeping the selected commit selected. The commit and graph rows are shared with the rows being drawn, so a frame copies no commit: this is a measured scenario on a large repository.

`ide-vcs` is a direct dependency now; the ported VCS models still carry their own mirrors of its types and are to be moved onto it as they are wired. `chrono` (the version the backend locks, `clock` only) gives the local UTC offset at a commit's moment, so a winter and a summer date each show their own local time.

Restoring a project's tabs does not take the keyboard: a restored tab's editor loads without focus, which stays in the Project view as on any open (the webview focuses only when tabs were already live). The first version of the restore did take it; an assertion in `workbench::tests::a_projects_tabs_and_carets_are_restored` holds this now.

Not in this slice: the branches tree, filters, commit details and changed files, context menus, column headers and resizing, long-edge and linear-branch options, multiple repositories.

## D31. A focused view may claim keys before the keymap; the terminal does

The shortcut dispatcher sees every keystroke before the focused view (D12). The terminal needs the opposite for its own keys, as the webview's terminal key handler runs before its keymap: Ctrl+C must interrupt the program, not run an IDE action. `platform::input::claim_keys` lets a view see keys first while its focus handle is focused; the terminal's claim applies the ported rules (`terminal::keys::route_terminal_key`): copy and paste, ⌘K, ⌘W, ⌘F, the shell's line-editing strokes for ⌘ and ⌥ with the arrows, Ctrl with a letter by the letter it types, Escape, and Settings › Terminal › Override IDE shortcuts and Escape-to-editor.

⌘K clears inside the emulator as xterm.js does (the cursor's line becomes the first, the rest and the scrollback go) and does not involve the shell. A first version sent the shell a form feed after clearing the scrollback; measured in the real-shell test, the shell's redraw scrolled the old screen back into the scrollback.

The terminal's colours now come from the ported `terminal::theme::build_theme` (including the palette derived for themes without ANSI tokens), replacing the view's own reduced copy. Still the view's own and to be replaced by the ported modules as the tool window grows: tabs and groups (`terminal::store`), the resize debounce (`terminal::fit`), links, the find bar, drop and image paste.

## D32. Find in Files: the models are in; their search types are the UI's own, converted at the edge

`find/` holds the ported Find in Files and Replace in Files models (query, scopes and file masks, a find session, the Find tool window's tree, the replace flow, the search popup's keys): 60 spec cases, no view yet.

`find/ipc.rs` keeps UI-side copies of the backend's search types (`ide_search`, `ide_index`, `ide_lsp`), with paths as the `/`-separated strings the models compare and with equality and defaults the tests rely on. They are not replaced by the backend's types: the copies are what the models were specified against. The view that will call the backend converts in both directions in one place, where the compiler checks every field.

Delivered by the same subagent but not wired: its Search Everywhere controller, routes and a second copy of the model, ranking and filters, which duplicate the Search Everywhere batch another subagent is still writing. They stay out of the build until the two are reconciled into one.

## D33. An own one-line text field; Find in Files starts in the Find tool window

`ui::text_field::TextField` is the app's single-line input (query boxes, later dialog fields): its own text, caret, selection, clipboard, word and line movement, and the OS input method through GPUI's input handler. The component library's input was not used: it brings its own styling and key contexts, and the dispatcher already knows `TextField` as the context where plain typing stays in the field.

`find.inPath` shows the Find tool window with the query focused, where the webview opens the Find in Files popup (query, scope, file mask, preview) and only sends results to the tool window on request. This is a deliberate first slice, not parity: the popup, scopes, file masks, the preview, Replace in Files and result tabs are not there. What is there is driven by the ported models: `FindSession` validates and runs the search through `ide_host::ops::search`, results stream in by file, the case, word and regex options search again, and Enter or a double click opens the match with the caret on it. `find::backend` is the one place the UI's search types and `ide_search`'s are converted (D32).

## D34. The Terminal tool window has tabs; a tool window says where its keyboard goes

`terminal::panel::TerminalPanel` drives the ported tab model (`terminal::store::Terminals`): it creates a terminal for each session the model adds, drops it when the model disposes the session (which ends the shell), and writes the remembered tabs under the webview's key (`openidea.terminal.tabs.v1:<project>`). New tabs take the next default name (`Local`, `Local (2)`), a shell that exits closes its tab when Settings › Terminal › Close session when it ends is on, the last tab closing hides the tool window, and showing the Terminal without a tab starts one: all as the webview does.

The workbench no longer keeps a fixed focus handle per tool window. It asks the window's view each time (`Focusable`), because the Terminal hands the keyboard to whichever tab is showing.

A tab of a shell keeps its own name at the prompt and while a command runs. The webview shows the running command's title, which it knows from shell integration's command marks (OSC 133); `alacritty_terminal` does not report those, so this is a gap until the marks are parsed here. Splits within the Terminal are in the model and restored from storage, but only the active group is drawn. Not there yet: renaming, drag and drop between groups, the tab context menu, predefined sessions, plugin sessions, restart on Enter after exit when Close on exit is off.

## D35. Tests that start a shell give it a home of its own (after they had written to the owner's history)

What happened: the terminal tests start the account's login shell on a real PTY and type commands into it. Until this change the shell was the owner's own interactive zsh with the owner's startup files, so every run appended the tests' commands to `~/.zsh_history`. Measured on 2026-11-12 with `grep -cF`: 34 lines `echo marker-$((6*7)); stty size`, 34 `echo pasted-$((40+2))`, 30 `exit 3`, 29 starting `echo in-$(basename`, 10 `sleep 30`, 10 `echo back-$((1+1))`, 2 `echo second-$((1+1))` (about 150 lines of 10,840), plus a few bare `exit`. Nothing else of the owner's was written. The history file was not edited: it is the owner's, and the lines are harmless to leave or to delete.

The fix: `terminal::test_support::isolate_shells` sets the global `terminal.env` of the test's temporary settings to a `HOME` and `ZDOTDIR` under the test's temporary directory (with an empty `.zshrc`) and `HISTFILE=/dev/null`, before any terminal starts. Every test that starts a shell calls it. Verified: the marker count stayed at 34 across the full suite run twice.

The app itself is unaffected: a real terminal is meant to be the user's own shell.

## D36. The diff models bring their own comparison engine

`vcs_diff/` holds the diff viewer's models (hunks and gutter marks, navigation, options, scroll sync, stripes, conflict markers, inline blame, compare) and `vcs_diff::comparison`, a port of the webview's IntelliJ-style comparison (by line, word and character, with its chunk optimiser and move detection), checked against the webview's 172 fixture cases (`ideaCases.json`, read as-is). No diff view is drawn yet.

Choices the subagent made, kept: the web editor's own fallback diff is not ported, so a comparison too large for the engine degrades to one coarse chunk between the shared leading and trailing lines; blame ages are English "N units ago" through the text catalog instead of the browser's relative-time formatter, so other locales read the catalog's wording; three-way merge chunks are not computed here (the merge-stripe cases use hand-built changes).

Subagent choices in earlier batches, recorded here as they are judgment calls too. Search Everywhere: a translated action's English title matches through its keywords, with their lower weight, where the webview has a branch of its own for the English title. Version control: commit messages wrap at the margin counting characters, where the webview counts UTF-16 units (they differ only beyond the Basic Multilingual Plane), and commits not loaded yet sort after loaded ones when ordering newest first.

Search types still exist twice (`find/ipc.rs` and `everywhere/model.rs`: symbol and line matches, workspace symbols). D32 justifies one UI-side copy, not two: they are to be merged into one when Search Everywhere gets its view.

## D37. Costs that grow with the project are paid once

Three places did work proportional to everything loaded each time a little more arrived; all three are measured scenarios or count towards them.

- A terminal tab's shell starts when the tab first shows, as in the webview ("hidden tabs stay unopened"): tabs remembered from the last run cost no process until selected. `terminal::panel::tests::remembered_tabs_start_their_shells_when_first_shown`.
- The Git log continues the graph from the lanes left open by the last page instead of recomputing it from the first commit. `vcs::log_view::tests::a_graph_continued_page_by_page_is_the_whole_graph` checks that the two are the same for every way of splitting a branching history.
- Find in Files adds rows for the files found since the last batch instead of rebuilding every row. A new search keeps the previous rows until it has a result of its own or ends, as the session model intends.

## D38. Search Everywhere, and the popup slot it opens in

`everywhere/view.rs` is the first overlay in the app, opened by the six `navigate.*` actions on their tabs. `SeController` decides which sources a query needs; the view asks `ide_host::ops::nav` (`find_files`, `find_symbols`, `recent_files`) and `ide_host::ops::search::start` for them and feeds the answers back. The webview's rules that decide what Enter opens are kept: the query rests 60 ms before it is searched, the selection is the top result until the user moves it, and Enter typed before the top result is final waits for it instead of opening a row of the previous query.

The workbench has one popup slot (`open_popup` / `close_popup`) for this and every overlay to come. It draws the popup over the whole window (`deferred`, on an occluding backdrop that closes it on a click beside it), remembers what had the keyboard and gives it back on closing (the editor when nothing had it), and publishes the context keys as it closes. The last part fixed a bug the first version had, found in review and reproduced by a test: an action chosen in the popup ran while the keys still said the popup was open and nothing was focused, so one that needs `terminalFocus` was refused. `search_everywhere_gives_the_keyboard_back_and_runs_actions_where_it_was_opened` fails without the republish.

Choices:

- The popup claims Enter, Tab, the arrows and Page Up/Down in every modifier combination while its query has the keyboard, as the webview does (`claimSearchKeys`), so the keymap's bindings for them do not run. The combinations whose behaviour is not ported (⌥↵ menu, ⌘↵ to Find in Files, ⌥↑/⌥↓ history, ⇧↵ to the side, which opens in place) are claimed all the same: doing nothing is closer to the webview than running an editor action behind the popup.
- Sources the app cannot answer yet reply at once instead of staying "loading": language-server workspace symbols answer "no server", endpoints and settings pages answer "failed", which the controller treats as empty. Recent actions are an empty list. Each is listed as a gap in the `search.everywhere` overlay row.
- An action that shows a state (a toggle) leaves the popup open after it runs, as in the webview; any other result closes the popup first and then acts. Whether an action is a toggle is asked, and the action run, after the workbench's own update is over (`window.defer`): both may read the workbench, and asking inside the update panicked in the test.
- A plain click on a row selects and opens it, as in the webview (`List` calls `onActivate` on a single-selection click). Its modifier clicks, which select several rows, are not ported.
- The search types that existed twice are now one: `everywhere::model` uses `find::ipc::LineMatch`, and the copies of `SymbolMatch` and `WorkspaceSymbol` nothing in `find/` used are gone. `ide-index` is a direct dependency, to name the types `ide_host::ops::nav` returns.

## D39. The menu bar is drawn from the Tauri app's table

`src-tauri/src/menu.rs` already holds the menu bar as a pure table (`spec(os)`) with the functions that route an item and say whether it is enabled; the rest of the file draws it with Tauri. `docs/gpui-port/menuspec.mjs` writes those pure parts to `app-gpui/src/workbench/menu_spec.rs`, and with `--check` fails when the written file differs, so both apps show the same menus and the table is edited in one place. The alternatives were to move the table into a crate both apps use, which means editing the Tauri app that this work has left untouched, or to copy it by hand, which drifts. The script formats what it writes with rustfmt, as the commit hook does, so the comparison is byte for byte against the committed file. When the Tauri app is retired the table simply stays where the script wrote it.

`workbench/menu.rs` does what the webview's `menuBridge.ts` and the drawing half of `menu.rs` did between them: the focused window's state (`MenuState`) plus the table give the bar (`model`), which is installed with GPUI's `set_menus` only when it differs from the one shown, 120 ms after the window last changed.

Choices:

- An item shows a shortcut only through a GPUI key binding for its command, so the bar binds each shown shortcut to `MenuCommand`. The keymap stays the only dispatcher: the keystroke interceptor marks a key the keymap let pass as "in flight", and a command that arrives while one is in flight is dropped and the key goes on to whatever has the keyboard. Only the items whose shortcut the table itself gives (Quit, window cycling) and the OS roles the keymap never binds (Hide, Hide Others, Minimize) run from the key. `the_menu_bar_follows_the_window_and_runs_chosen_items` fails without the drop. That covers the key GPUI dispatches itself. On macOS there is a second path, read in `gpui-pre-macos` (`handle_key_event`): a key equivalent that nothing in the window handled is reported unhandled to AppKit, which then performs the menu item with that shortcut, in a later cycle when the flag is already cleared. So a chosen item is also dropped when AppKit's current event is a key press, which is how the Tauri app tells the two apart (`handling_key_press`). This second path is not exercised by any test: the test platform has no AppKit menu. It is a reading of the source plus the Tauri app's own practice, to be confirmed by pressing a declined shortcut in the running app.
- The bar follows the focused window only when something it shows could have changed: the windows' context keys are counted as they change, as are settings changes and the recent projects, and a draw that finds the same counts asks for nothing. The table for the OS is built once and the labels are shared, not copied. `the_menu_bar_follows_the_focused_window_when_it_changes`.
- Bindings from an earlier keymap are left bound: GPUI shows the last binding of a command, and a stale one can only fire a command that is then dropped. Clearing all bindings would also remove any other GPUI bindings the app gains later.
- Undo, Redo, Cut, Copy, Paste and Select All are the OS's roles on macOS. Chosen from the bar they run the editor's `edit.*` action in the focused window. A text field that is not the editor does not get them from the bar yet (its own keys work).
- Check for Updates is left out, as the Tauri app does when no updater is configured: this app has none.
- `app.quit` is registered here because the bar is its first user: each window is asked to close, and the app quits when all agree. A window with unsaved changes asks its question and, once answered, closes itself, but the app does not go on to quit. Marked partial.

## D40. Windows are labelled and opened as the Tauri shell does it

`workbench/windows.rs` opens the app's windows (`main`, then `project-<n>` with the lowest free number, the Tauri shell's labels and order) and decides where a project opens: the window that already has it comes to the front, a window showing nothing takes it, and one with a project follows `general.openProjectIn` or asks. `main.rs` opens its first window through it.

Choices:

- The question is the OS's own prompt, so it has This Window, New Window and Cancel but no "Don't ask again" box, and no Attach (attaching a project to a workspace is not ported). The stored setting is honoured when it is set elsewhere.
- Replacing a window's project saves what is unsaved and writes the editor session first, then lets the backend swap the project (`Host::open_project` closes the previous one itself). Close Project uses `close_project_detached`: it runs on the UI thread and must not wait for language servers to stop.
- An opened project is recorded as recent where it is opened (in the workbench, after the backend opened it), with its branch and display name as `AppState::record_project` does, and Open Recent is read again then.

## D41. The Project view's operations are models first

`project_view/` holds what the Project view does beyond listing folders: its panes and view modes, selection and inline editing, clipboard, contributed roots, the file operations behind its actions (new, rename, move, copy, delete with usages, undo), its context and New menus, Select In and module groups. It is a subagent's port of fifteen specs (122 tests), written without a compiler and wired here; it built on the first try and needed three lint fixes. `ide-build` is a dependency for `GradleProject`.

Choices the subagent made, kept:

- The state machine of the tree (load, file-system change, reveal, toggle) exists twice for now: `project_view::store::ProjectViewState`, which the operation specs drive, and the one `project_tree.rs` draws from. The view is to move onto the store when its actions get handlers; until then the overlap is a known cost, not a design.
- The import and Unity refresh specs mock backend calls; here they are checked on the pure models (`LocalImport`, `UnityRefresh`) and marked partial.
- Menu rows reuse `vcs_ops::menu::MenuItem` and are identified by command or label, since it has no id; the Select In header is a title field.
- Not ported: the file refactoring dialogs (only their request and the undo ordering), the React component specs, and every action handler: `actions.rs` computes the actions' titles and targets but registers nothing.

## D42. Appearance options are attributes of the theme

The Appearance options the webview sets as attributes on the document root (`data-compact`, contrast scrollbars, red-green palette, a different tool window background, project colours, menu icons) are passed as attributes when the theme is cascaded, so the stylesheet's own blocks for them apply and nothing is restated in Rust. The four View › Appearance toggles flip their global setting; the status bar and the tool window stripes are drawn only when theirs is on. The four Stretch actions have ids built from a direction in the webview, so they are registered with their metadata written out instead of read from the generated list.

IDE zoom is not ported: sizes come from tokens and from literals in the views, and scaling only the tokens would zoom half the UI. It needs one scale applied where GPUI resolves pixels, which is a change of its own.

## D43. Views written by subagents: File Structure

`everywhere/structure_view.rs` is the first view (not model) written by a subagent without a compiler, against `everywhere/view.rs` as its template; it was wired here, built on the first try and its twelve tests passed, one of them against the real backend (a Makefile's outline, which needs no language server). It is the ⌘F12 popup only: the Structure tool window is a separate component in the webview and is not ported.

Choices the subagent made, kept:

- The popup loads the outline once when it opens and again when inherited members are toggled; the webview also reloads it 500 ms after an edit, which cannot happen under a popup that has the keyboard.
- The selection goes back to the symbol at the caret (or the best match) whenever the shown tree changes, as the webview's effect does: after a sort or visibility option, and when inherited members arrive.
- Keys are claimed where the webview prevents the default: Enter, Up and Down always; Right when the selected node has children; Left when something is selected; Escape only while there is a query, which it clears.
- The selected row uses the inactive selection colour, since the keyboard is in the query field, not in the tree. Search Everywhere's rows use the active one; one of the two is to change when the popups get a common list.
- A supertype's file is opened and closed again in the backend to read its outline, which marks it as recent. (The subagent built the type-hierarchy request from JSON because `ide-lsp` was not a dependency then; it is now, and the view uses its types.)
- An outline that fails to load shows "No symbols" and logs the error, as the webview's popup does.

## D44. Recent Files and the Switcher

`everywhere/recent_view.rs` is one view for Recent Files (⌘E) and the Switcher (⌃⇥), as `RecentPopups.tsx` is one component; a subagent wrote it without a compiler. It built on the first try; one of its seventeen tests failed, and the measured cause was the test's hand-traced expectation (which file row the presses last touched), not the code, so the test was corrected.

What the workbench does for it:

- A tab's editor becoming the active one is counted (`activations`), which is the order the Switcher lists open editors in, and its file is marked recent in the backend then, as the webview does on every change of the active editor. Without the second, ⌘E and Enter would not go back and forth between two open files. `recent_files_and_the_switcher_go_between_the_editors` checks both.
- Closing a tab from the popup does not move the keyboard out of it; hiding a tool window from it gives the keyboard back to the popup after the layout handed it to the editor.

Choices the subagent made, kept: the popup's keys are claimed ahead of the keymap, so ⌃⇥ inside it is always the popup's; releasing Ctrl opens the selection only when Ctrl was seen held, so a Switcher opened from the menu stays open; tool window titles are shown untranslated, as the tool window headers are.

Not there: Recent Locations (its link closes the popup), the set of files edited in this session, opening to the side, and keeping a row when its tab refuses to close (with autosave on, the default, a tab never refuses).

## D45. Settings and templates as models; the backend's settings are the schema

`settings_model/` (22 files) and `templates/` (17 files) are a subagent's port of 29 specs, 260 tests, written without a compiler; wiring them needed two one-line compile fixes and eleven lint fixes. No Settings dialog and no template expansion in the editor exist yet: these are the models they will be drawn from.

- The webview validates settings itself (`validateSettings`: a malformed value becomes the default, text is trimmed, numbers clamped, unknown names dropped). Here the settings are `ide_core::settings::Settings`, the backend's own types, read through serde: a value of the wrong type is refused instead of defaulted, and text and numbers are kept as stored. The specs that test the webview's repairs (`agents`, `cmake`, `goBuildTags`, `behavior`) are marked partial for that reason rather than given a second validator that the backend would not agree with.
- Syntax-tree cases of the templates (the context a template applies in, postfix targets) are driven with hand-built node chains, not a parser; the editor is to pass its real nodes.
- Emmet expansion is behind an `EmmetEngine` trait with no implementation: the webview uses the `emmet` package's tables, and porting those is a job of its own.

## D46. Run configurations as models

`run/` (37 files, 268 tests) is a subagent's port of twenty of the run feature's specs: the configuration store and its persistence, the built-in configuration types for the JVM, Go, CMake, Gradle and .NET targets, the console with its ANSI handling and links, build commands and tasks, update policies of a running application, and navigation from console output. It built on the first try and needed six lint fixes. `ide-lsp` is a dependency now (Java launch information and hierarchy items); the lock check still passes.

Not ported, and the largest gap in this area: the producers that make a configuration from a file or from the caret (`producers`, `producers.ruby`, `contextRun`, `npmGutter`, 113 cases), the Ruby, PHPUnit, .NET test and Tomcat types, and the test runner's parsers. No Run tool window, run widget or gutter icon is drawn yet.

Choices the subagent made, kept: the store hands completed console lines back to its caller instead of keeping test parsers itself; console links reuse the terminal's URL trimming, which also drops an unbalanced closing bracket; index and language-server results the specs mock are plain structs of this module (`FoundSource`, `SymbolHit`) until the views pass the real ones.

## D47. A spec row is checked against the tests it cites

Subagents report how many cases of each spec they ported, and the rows were first marked from those reports with the test modules guessed from file names. `inventory.mjs` now refuses a spec row marked done when the tests it cites are fewer than the spec's cases, counted over all rows that cite the same tests, unless the proof says after a semicolon why fewer suffice. Running it found four groups: three where the cases existed but in modules the proof did not cite (corrected), and one of my own early rows where several cases are one test (now said in the proof). A second pass matched every case title against Rust test names; it found no missing case in the subagent batches, and two rows whose "covered elsewhere" claims could not be found by name, which are now partial (`settings keymap`, `settings languages`). The check is a lower bound on honesty, not a proof of equivalence: it cannot tell a test that asserts the wrong thing.

## D48. The debugger as models

`debugger/` (34 files, 274 tests) is a subagent's port of 28 of the debugger's specs: the session state with its console, frames, values and execution point, the actions (start, attach, step, rerun, hot swap), breakpoints of every kind with their store, the Java launch path, remote and attach configurations, start failures and the missing-adapter flow, and the editor-side models (gutter, run to cursor, smart step, quick evaluate, value tooltips). It built on the first try and needed four lint fixes. `ide-dap` is a dependency now; the lock check passes. Its rows were marked from a match of every spec case title against the test names, not from the subagent's counts (D47).

Not ported: the Debug tool window and its views, the console's links and find bar, expression completion, and the cases that test the web editor's DOM (the breakpoint click area, an inline rendering error). No debugger UI is drawn yet.

It depends on `run::{java_launch, ansi, store, config_types, configs}`: the subagent dropped its own copies of those in favour of the run batch's.

## D49. Editor panes; editors of one document tell each other of their edits

The editor area draws the tab model's layout: every pane has its tab strip and the editor of its selected tab, splits share their room evenly with a line between the panes, and the pane whose editor takes the keyboard becomes the active one. The split actions (Split Right and Down, Unsplit, Unsplit All, Change Splitter Orientation, Move to and Open in Opposite Group, Go to Next and Previous Splitter) run the model's operations.

A file shown in two panes has two editors on one backend document. I first assumed they would follow each other through the backend's `DocPatched` event and the test refuted it: the backend announces only the edits it makes itself, not the ones an editor sends. So an editor now tells the other editors of its document about each edit it made (`DocumentEditors`), and they follow it exactly as they follow a backend edit, selection included. Each editor keeps its own undo history, in which the other pane's edits are entries of their own.

Not there: splitters cannot be dragged (the model keeps no sizes; the panes share evenly), and tabs cannot be dragged between panes.

## D50. One popup menu for every context menu

`ui/popup_menu.rs` draws the menu model the ports already share (`vcs_ops::menu::MenuItem<C>`): rows bound to a registered action, which take the action's title, icon, shortcut, enabled and toggled state, and rows of the menu's own that carry a command `C` back to whoever opened the menu. The workbench opens one with `show_menu` in the popup slot, so it gets the slot's focus restore and context republish, and a chosen action runs through the same deferred path as one chosen in Search Everywhere.

The editor tab's menu is the first user (`tab_menu`, `run_tab_command`), with the webview's rows in its order. Rows bound to actions that are not registered yet (Compare With, Copy Path, Rename File, Configure Editor Tabs) do not show, exactly as the webview's `resolveMenuItems` drops an unknown action; a submenu left with no row is not offered either, which the webview never has to decide because there all of them are registered. Not in the menu yet: Close All Read-Only Tabs (no document metadata is kept per tab), Open in New Window, and the groups other features contribute (Run, Local History, Git, Open In, Bookmarks).

The pointer opens a submenu after 120 ms on its row and closes another after 200 ms, as in the webview. The webview also keeps a submenu open while the pointer moves diagonally towards it; that needs the submenu's drawn rectangle, which this view does not measure yet.

## D51. Presets keep their declaration order (a bug in rows that were marked done)

Where two actions share a key, the webview tries the one declared later first (`effectiveBindings` walks the preset in object order, and dispatch takes the last pair whose action is enabled). `gen-data.mjs` sorted each preset by action id before writing it, and the port held presets in a sorted map, so shared keys resolved alphabetically: for example New File could take ⌘N from Generate, and New Branch ⌥⌘N from Inline. The six "Keymap presets" rows were marked at parity on a test that only checked that every binding is well formed. A subagent porting the keymap specs found it by reading; I confirmed it in the generator's source.

Fixed: the generator writes each preset as pairs in declaration order, `preset_order` exposes that order, and the effective keymap walks it. A user override of an action the preset binds keeps the preset's place with the user's keys, which is what JavaScript's object spread does in `{...preset, ...overrides}` (my earlier test asserted the opposite and is corrected); an action only the user binds comes last. `lets_scoped_actions_win_shared_keys` carries the spec's fourteen pairs for both IntelliJ presets.

Lesson kept in D47's spirit: a row is not at parity because a test exists that mentions it. The preset rows now cite the order tests too.

## D52. Platform and workbench models; specs that test the browser

`platform_more/` (24 modules, 142 tests) is a subagent's port of the platform and workbench specs that were still open: inspections and their controls, remote sessions and which settings go to the remote side, deployment and workflow settings, the .NET project reading, the project session, boot failures, distraction-free mode on the real tool window layout, file drops, file editor and provider registries, running processes, pointer drags and copyable text. It built on the first try with no lint fixes.

Some specs test the web platform itself and have nothing to port: WebKit's double-click and tap ordering quirks (`doubleClick`, `reordered-tap-click`), DOM caret handling on focus changes, script load errors, CSS class names, Tauri channel encoding. Those rows stay unchecked with that reason where they are whole files, and are named in the proof where they are cases of a ported spec. GPUI reports click counts and has no such reordering.

As in D45, settings validators are normalisers over the backend's typed settings; a value of the wrong type is refused by serde rather than defaulted.

The same subagent found the keymap order bug (D51).

## D53. Go to Line and bookmarks

`editor/goto_line.rs` and `everywhere/bookmarks_popup.rs` (the Bookmarks popup and the mnemonic chooser) are a subagent's views; both built on the first try and their 33 tests passed, including the ones the subagent expected to fail. The workbench keeps the project's bookmarks (`BookmarkStore`, read from the UI storage when a project opens) and registers the actions that are not popups: toggle at the caret or at a given line, toggle and go to the ten digit bookmarks, next and previous, file bookmarks.

- The popups work on their own copy of the store and report what to save; the workbench takes the copy back and writes it. That is how the store model is meant to be used and keeps the views free of the workbench.
- Go to Line is a popup in the popup slot, so a click beside it closes it; the webview's is a dialog that stays. Numbers typed beyond half of `u32` saturate, where the webview passes any digits on and clamps when it opens the place.
- The Bookmarks preview shows the file as plain text with the bookmarked line marked. The webview's preview is a second editor on the document. The file is read without opening it as a document, so that previewing does not make it a recent file.
- Editing a description happens in a row of the popup instead of a second dialog, since the slot holds one popup.
- After Delete the selection stays at the same index, as in the webview, even when that is now a header.

Not there: the Bookmarks tool window, bookmark marks in the gutter, and bookmarks following lines as the text is edited (the model has `set_file_lines`; the editor does not call it).

## D54. Find in Files is the popup again

`find/popup_view.rs` is a subagent's view of the Find and Replace in Files popup on the ported models (`FindSession`, `store`, `replace_flow`, scopes). About 3,800 lines written without a compiler built on the first try and its 23 tests passed. `find.inPath` and `find.replaceInPath` now open it, as in the webview; before, `find.inPath` showed the Find tool window (D33), which stays as the place results are kept: Open in Find Window hands the popup's query to the panel (`FindPanel::run_query`), with its scope and masks for that one search.

Choices, the subagent's unless noted:

- What the popup remembers between openings (options, scope, mask) is kept per window in a process global and written when the popup goes away, because the popup slot drops a view without asking it. If the workbench is to own that state later, it is one input field and one accessor away.
- Replace All asks through the OS prompt and checks afterwards that the results are still the ones asked about.
- The popup's keys are claimed ahead of the keymap; ⌘G, ⇧⌘G, F4 and ⌘↓ are left to the `find.popup.*` actions and the `findPopupOpen` context key, which the workbench now publishes.
- A pinned popup stays open when a match is opened and when the click is beside it (mine: the popup slot's backdrop asks the Find popup whether it is pinned).
- The view's tests were written from the component; the webview's own popup spec (25 cases) was not in the subagent's brief and is marked partial, not done.

Not there: see the `search.findInFiles` overlay row; the largest are the preview editor and multi-line queries (the text field is one line).

## D55. Status bar items are buttons

The editor's three status bar items answer a click as in the webview: the position opens Go to Line, the line separator and the encoding open a menu at the pointer (the popup menu keeps itself inside the window, so it opens upwards from the bar). The rows call `Documents::set_line_ending` and `set_encoding` through the editor, and a refusal by the backend (reloading with unsaved changes, a read-only file) is told in a notification under the webview's titles.

The check mark of the current line separator is drawn as the row's icon: the shared menu model has no "checked" for rows of a menu's own, and adding the field while subagents write against the type would have broken their files. It is to become a field when the menus with real toggles arrive.

## D56. Run producers, and a stop that was not honoured

`run_producers/` (14 files, 184 tests) completes the run feature's models: the producers that make a configuration from a file, the caret or a directory for every language the webview knows, the context Run/Debug menu, npm script gutters, runtime and tool lookup, Ruby tests, stack traces, service order, console folds and external tools. It built on the first try.

Its one failing test was a real bug in the earlier run batch, which the subagent had predicted from reading: in `StartRun::java_prepared`, a successful classpath answer that arrived after Stop was pressed went on to launch the program. The webview's spec says the opposite ("cancels the request and ignores a late success response"). Fixed in `run/store.rs`: whether the launch was aborted is asked before the answer is looked at.

Choices the subagent made, kept: what the producers read from global stores in the webview (Go modules, Python packages, the Gradle model, tools on the PATH) is passed in as data; the context menu has rows of its own, since the shared menu model has no header rows; the test-state icons use a small model fed with results, because the test tree is not ported yet (the test runner batch is still being written).

## D57. The rest of the version-control logic, with the three-way merge

`vcs_more/` (28 files, 185 tests) is a subagent's port of 26 of the 27 version-control specs that were open: the three-way merge (its diff3 and the model of the merge editor with accept, ignore, undo and redo), loading the Git log page by page and going to a ref, its filters, search and branch memory, several repositories in one project, show diff, push targets and a rejected push, worktrees, recent changes. It needed one compile fix and one lint fix, and four small changes in existing modules (two comparison modules and one function made visible, `GitLog::view_of_mut`). `pluginCommit` (3 cases) is not started.

Choices the subagent made, kept, with the one that matters most first:

- The webview's diff3 runs on CodeMirror's diff; here it runs on the crate's own comparison (`comparison::lcs` and the chunk optimiser). Where a difference has several equally short answers the boundaries of the clusters may differ from the webview's. The 23 spec cases pass, including the symmetry one; a merge that is ambiguous in that way could still be cut differently. The webview's 3 s and 1 s diff timeouts are not kept.
- Every transaction of the merge model is one undo step; CodeMirror groups adjacent typing.
- White space in word and blank patterns is JavaScript's set written out, not the `regex` crate's.
- An issue-link pattern that needs look-around or backreferences is skipped, as an invalid one is.
- Loading the log is a set of step machines the view drives (`PageLoad`, `GoToRef`); session restore, bookmark sync and revealing a commit are left out of the store.

No merge editor, diff viewer or multi-repository UI is drawn from these yet.

## D58. Notifications are drawn

`workbench/notification_balloons.rs` and `workbench/notifications_view.rs` are a subagent's views on the notifications model; both built on the first try and their 25 tests passed. The workbench owns the model and keeps the two views in step with one routine (`sync_notifications`), called wherever a notification is raised, dismissed or fades and whenever the Notifications tool window shows or hides. A timer wakes it for the next balloon due to fade.

- The views follow the component, not my brief, in three places the subagent checked in the source: a long body scrolls instead of expanding, the list is flat and newest first, and the tool window is reached from its stripe button and `view.notifications`; there is no bell in the status bar.
- Settings › Notifications are read now (they were passed as "none" before): the effective settings, serialised, give the model's settings. "Don't Show Again" writes the group's display to the global settings; the workbench test checks that the next notification of the group raises no balloon.
- A link of a notification runs its command when the command is a registered action. Commands that carry arguments in their own form (a run configuration, a repository) have no runner yet and are logged; the notification still shows the link.
- Balloons are drawn in a deferred layer below the popup slot, so a popup covers them, as the stylesheet orders them.

## D59. The test runners' logic

`run_testing/` (20 files, 174 tests) is a subagent's port of 12 specs: the parsers of test output (JVM, Jest/Vitest, pytest, unittest, Go, Cargo, .NET TRX, PHPUnit, Ruby, TeamCity messages, plugin runners), rerunning failed tests, the comparison of expected and actual, test states, and the Services tool window's run configurations with their sessions. It built on the first try; the integrating agent fixed two lints and changed no test expectation. TRX files are read with a small XML reader written for them, since the crate has no XML dependency and the format is fixed. None of it is drawn yet: the Run tool window's test tree and the Services tree are still open.

## D60. The Welcome screen

`workbench/welcome.rs` and `welcome_model.rs` are a subagent's view and model (43 tests); a second agent wired them into the workbench, which owns the view while the window has nothing open and drops it when a project or a file opens.

- The list is read again from storage when the window becomes active, when a project closes and when an open fails, since another window may have changed it. After a failed remove or a failed save of the groups it is read again too, so the screen never shows a change that was not stored (the agent's addition; the component keeps its optimistic state).
- While an open has failed the window shows the error, not the Welcome screen, and the hidden view does not take the keyboard.
- `welcome.recentProjects` and `file.openRecent` put the keyboard in the screen's search. In a window with a project they do nothing yet: the Recent Projects popup is a separate view.
- Not built: the Customize and Plugins pages of the screen, and reopening the last project at start (the logic is ported, nothing calls it, because the first window is opened from the command line in `main.rs`).

## D61. The Run tool window

`run/view.rs`, `run/driver.rs` and `run/sessions.rs` are a subagent's slice, built and tested by it (37 new tests, one of them end to end with real processes). The view reads the run store and reports what it wants; the driver carries out every step of a start against the backend and feeds output and exits back.

- Session tabs are drawn inside the view, as the Terminal's are, because a tool window's header has no slot for content yet.
- Close tab, clear, soft wraps, scroll to end and find are buttons of the view, not actions: the component has no ids for them either.
- Steps that need something not built answer in the way the component does without it: Gradle delegation resolves to the language server's classpath (no build model), a Build step or a before-launch build task without a build action ends with "Building is not available", an unknown external tool fails the launch. The SSH password retry of run targets is not built: such a start fails with the backend's message.
- `run.debug` is not registered. Its fallback is to run without the debugger when no debug action exists, which would make Debug a plain Run until the debugger is drawn.
- With "Current File" selected, which is the default of a project without configurations, `run.run` hands over to `run.runContext`; that action is not built, so Run does nothing visible there and logs it.
- Runs are stopped when their project closes. Stopping them when the window closes is not built.
- The console has no mouse selection (Select All, Copy and Copy All work on the whole text), cuts long lines when soft wraps are off, and shows "Emulate terminal" output as plain lines.

## D62. The editor's editing commands, as functions

`editor_commands/` (33 files, 263 tests) is subagents' work on 25 specs: comments, duplicate and delete line, move statement, occurrences, complete statement, join and split lines, case, humps, smart keys, tab and backspace, the clipboard with string escapes and paste reindent, column mode, vertical motion, folding, structural selection, move element, unwrap, doc comments, sort and reverse lines. A command takes the text, the selections and the language's facts and returns the changes and the new selections; nothing in the editor calls them yet.

- The editor today has one selection, no syntax tree per document and no folded ranges. The commands take several selections, a tree of plain nodes and the folded ranges as arguments, so wiring them is the editor's next slice, not a rewrite of these.
- Vertical motion moves by document line: soft wraps and folds are not taken into account until the editor can give visual lines.
- Two fixtures in the tests were wrong when first compiled (a node span that counted the selection markers, one quote too many in a C++ raw string); the integrating agent corrected the tests after reading the specs. No command needed a fix.
- Not started: `multiCaret` (6 cases about which mouse gesture adds a caret; it needs mouse bindings in the editor).

## D63. Code intelligence, as models

`editor_lsp/` (32 files, 277 tests) is subagents' work on 33 of the 34 specs of the editor's language-server side: completion (query, rows, ranking, applying an item with its extra edits and snippet), formatting with its guard, diagnostics, intentions and the bulb, hover precedence, signature help, lenses, inlays, inline completion, imports on paste, links, go to declaration with its chooser and pending hint, mouse navigation, rename in place, navigation in configuration files, spelling. Each takes the server's answer as an argument and returns what to show or the next request; nothing in the editor calls them yet. It compiled without an error.

- `hint.rs` had a real defect found on reading by the second agent: a delay timer reported after the request had finished still showed "Waiting…". Fixed with a flag and a test.
- `spelling.rs` declares its own copies of the speller's three types, because the speller's crate is not a dependency of this one. They are swapped for the real ones when the editor wires spelling.
- Rename in place ends with the typed name and whether to commit; the rename request itself and its preview belong to a flow that is not written.
- Not started: `syntaxFallback` (50 cases: navigation, highlights and rename from the syntax tree alone), which needs that rename flow and a tree per document.

## D64. Find and Replace in the editor

`editor/find_model.rs` and `editor/find_bar.rs` (51 tests) are a subagent's model and view; they compiled and passed on the first run. The workbench keeps one bar per editor tab, draws the active tab's bar between the tab strip and the editor, and registers `find.inFile`, `find.replaceInFile`, `find.next`, `find.previous`, `find.openInWindow` and `find.scrollToResults`.

- The editor does not mark the occurrences: it has no decorations yet. The current occurrence is selected, and the bar exposes its matches for the editor to draw later.
- Replace All sends one edit from the first occurrence to the last, since the editor applies one range at a time. It is one undo step, as in the component, but the backend sees a larger change.
- "Open in Find Window" searches the file again through the backend. The bar's In Selection scope, its excluded occurrences and unsaved text are not carried over.
- The bar's multiple-cursor buttons ask for actions that no handler exists for yet, so they do nothing. Escape in the editor does not close the bar (Escape in the bar does). A window with a single file and no project has no bar.
- A query of several lines shows on one line with a mark per line break, because the text field holds one line.

## D65. Five features as models, and what a model-only row means

`database/`, `github/`, `agents/`, `app_models/` and `plugin_models/` (817 tests) are subagents' work on 84 specs, compiled by an integrating agent with about 25 small fixes. None of it is drawn or called yet.

- How these rows are marked: a `.test.ts` spec whose cases all have a named Rust test is at parity as a spec. A `.test.tsx` spec whose state and logic are proven on a model is partial until its view exists, whatever the count. The feature, tool window and action rows of these features stay open.
- The integrating agent found one real defect by a failing test and its sibling by reading: two GitHub models matched keys by their browser names ("ArrowUp", "Enter") where this crate's views pass GPUI's ("up", "enter"). Both fixed, with their tests.
- Four backend crates are now dependencies of the app crate (`ide-db`, `ide-github`, `ide-agent`, `ide-tools`); the lock check still passes.
- The plugin models use their own copies of four types of the extension crate, which is not a dependency and is not re-exported by the host. They are swapped when the plugin views are wired.
- The app-shell models describe what the specs demand of opening and closing; the workbench's own older code is simpler and differs (reported by the model's author from reading, file and line in the commit's code): opening a project has no Attach answer, no remembered policy and no leave guard; opening files into an empty window opens only the last; closing asks only about unsaved changes, not about running processes or exit confirmation. Replacing that code with these models is a slice of its own, so `windows`, `closeGuard` and `lightEditClose` are partial.
- GitHub has no operations in the host crate: they exist only as commands of the desktop shell. The native app will call the GitHub crate directly.
- Not started in these features: the agents' composer popups (21 cases), most of the Plugins page and contributed views (about 180 cases), `menuBridge` (15, the webview side of the native menu, which the native menu replaces), `host` (3, a build-info request that is a compile-time constant here).

## D66. "Model only" is its own status, and it does not count

The inventory now tells apart a row whose tests prove a behaviour of the application from a row whose tests prove code that nothing reachable from the application calls. `inventory.mjs` computes, from the sources, which parts of the app crate are reached from `main`, `lib` and the workbench (a unit is a top-level module or a child of one; a unit is reached when non-test code of a reached unit names it). A row marked done whose cited tests all belong to unreached units is drawn `[m]` and counted as "model only", not as at parity. No tag is stored: when a view starts calling a model, its rows become `[x]` by themselves on the next run.

- Why: after the model batches the total read 430 rows at parity, and more than half of that was logic no window draws or calls. The advisor called the number misleading, and it was. With the split it reads 184 at parity, 242 model only.
- The reachability is by name, so it can err: a missed form of reference marks live code as model only (the safe direction), a child module whose name also appears for another reason inside its own parent may be counted as reached. It is an estimate at the level of files, not of functions: a reached file can still hold functions nobody calls.
- The four Find actions went back to partial: the bar works, but the editor does not mark occurrences, Escape in the editor does not close the bar, and a single-file window has none.
- A third browser key name was found by a search of the model directories (`is_chip_reset_key` in the GitHub list matched "Delete"); fixed. Briefs for models now say that keys arrive with GPUI's names.

## D67. The editor's core: several carets, one edit of several changes, marks, live commands

A subagent rebuilt the editor's core in one slice (20 new tests through the application; one old expectation changed).

- The editor keeps a list of selections with a main one. The old single-selection accessors stay as a view of the main range, so the Find bar, the status bar and every earlier test work unchanged.
- An edit is a list of changes with the selections to restore: one backend edit, one undo step, followed by the other editors of the document. Replace All uses it, which removes the merged edit of D64.
- Marks are ranges of a layer that follow edits until replaced. The Find bar marks its occurrences, the current one, its scope and the excluded ones, with the tokens of the component. With that, Escape in the editor closing the bar, and a bar in a single-file window, the four Find actions are at parity again.
- The editing commands run as actions (`editor/command_actions.rs`), each through the registry in a test that also checks one undo. A test asserts that the ids the tests run are exactly the registered ones, so a command cannot be registered without a test.
- No syntax tree. The backend has no tree of a document, only position queries and the highlight spans. The editor builds a small tree of comment and string runs from the highlight spans of the current version and gives it only to commands that ask "is this code, a comment or a string" (match brace, code-block navigation, move element, Enter in a comment, skipping a closing quote). Commands that need real structure get no tree and take their text path, so they are partial: Move Statement moves the line, Toggle Case does not protect escapes, comment toggling refuses a block comment that contains its own closer. `code.unwrap`, `code.extendSelection` and all folding actions are not registered.
- Copy without a selection now copies the caret line, as the clipboard spec demands; the earlier view test expected the clipboard to be kept and was changed (the spec's lines are in the report kept with the session).
- In a workbench window the keymap's bindings for the new actions are live, e.g. ⌘⌫ is Delete Line as in the IntelliJ preset.
- Not wired in this slice: auto-closing of brackets and quotes (so Tab-out of a pair never fires), string escapes on paste, dragging a column block with the mouse, the hints of "no further occurrence" and "nothing to expand" (logged only), detecting indentation from the file's text.

## D68. The Services tool window

`services/` (model, Docker model, view; 57 new tests) was written by a subagent without a compiler and compiled by another with two fixes; that agent then wired the window (`workbench/services_window.rs`) with one end-to-end test on real processes.

- Run configurations work end to end: adding a type, the tree, Run and Stop from a row or the toolbar, the detail pane following the session. The pane is a small read-only list of the session's console lines, because the Run tool window's console cannot draw one session on its own yet.
- Docker is read, not operated: settings and the snapshot are loaded off the main thread (a machine without Docker shows "not running" or "not installed"), the tree and menus are drawn, and the filters are stored. Container operations, pull/prune/compose streams, the dashboards and the connection and registry dialogs are logged and not carried out, so the thirteen `docker.*` actions that need them are not registered. A Docker task started from a menu would leave its banner up; this is a known gap, since nothing ends the task.
- No debugger: debug sessions of a service, Rerun Debugger and Show Debug are logged.
- Adding a type that has several configurations adds all of them: the picker is not built.
- One fix came from running the tests and reading the focus code: a double click opened the grid's cell editor and the grid took the focus back on the same press; the press is now marked handled.
- The views of the Database and Pull Requests features are compiled and tested (65 tests) but not shown by any window yet; their rows stay model only until they are.

## D69. Show Diff, and seven more batches compiled

An integrating agent compiled the diff viewer, the Settings dialog shell, the Debug tool window's views and four model batches (refactoring and navigation, documents and tabs, and 22 small features): 735 new tests, about 20 compile fixes, three real defects.

- Defects found by the tests: undo and redo of a workspace edit were both refused (an inverted condition in the history model); two more browser key names used as keys (macros, the Vim mode's Escape), now GPUI's names.
- Show Diff works from the Commit tool window: Enter or a double click on a changed file shows its diff, F4 opens the file, as the component does (two earlier tests pressed Enter to open a file and now press F4). The diff is drawn as a front window in a layer of its own, below the popup slot, so its toolbar menus open over it; a popup would have been closed by its own menus.
- A diff as an editor tab is not built. The editor area holds file editors only; a tab that holds another view is a foundation piece several features wait for (diffs, agent sessions, table editors, previews).
- `vcsAvailable` is published now (a project with a repository), which enables every action gated on it; most of those are not registered yet.
- The panes are read-only: applying a change from the viewer is not wired.
- The Settings dialog shell, the Debug views and the diff viewer's tests pass, but only the diff viewer is shown by the app; the others are model only until wired. The Debug views need a driver for the debug adapter; the Settings dialog needs its built-in pages, which are written and wait for the compiler.

## D70. Choosing and editing run configurations

The run widget sits in the title bar of a project window; the configuration chooser and the Edit Configurations dialog are live, on views a subagent wrote without a compiler (48 tests, no fix needed) and wiring by another (three end-to-end tests on a real project).

- The chooser is a popup. Edit Configurations is a front window in its own layer, like the diff: in the popup slot a click beside it, or any menu opening, would have dropped the working copy without telling the store the editor closed.
- Saving goes through the driver, which answers the save's module questions with real file searches, saves through the backend and waits for the reload. A failed save shows in the dialog and as a notification.
- A configuration added in the dialog is stored in the project's local file, as the backend decides for configurations marked local; the test checks that file.
- Debug is not offered anywhere in these views (the buttons follow whether the debug action exists), and `run.chooseDebugConfig` is not registered: every row would lead to an action that does not exist.
- The file dialog cannot start in a given folder with this GPUI version, so Browse ignores the field's current path.
- Not built: template editing, import of other tools' configurations, run targets, the environment table (one line with the same parsing instead), choices loaded from the backend, Tomcat settings.

## D71. Branches

The branch widget and the merge/rebase widget sit on the left of the title bar; the Branches popup, the prompt dialogs and the Push dialog are live (views by one subagent, wiring by another, four end-to-end tests on real temporary repositories with a local bare remote).

- The window holds one version-control state, read on the backend runtime after the project opens, after each operation, when the popup opens, and on the backend's change events with the component's debounce. The Commit and Git Log views keep their own reads; nothing was shared before, and merging the three is not part of this slice.
- Carried out through the existing models and the backend: checkout with the smart-checkout and diverged questions, new branch, rename, delete with its notice and Restore, merge and rebase (a stop on conflicts shows the title-bar controls; Abort, Continue and Skip work), update of a branch, pull into, fetch, push. Tests run checkout, new branch, rename, delete and restore, checkout of a revision, push, fetch, merge and abort; the others are written and not run by a test.
- Not carried out: anything across several repositories at once, Compare and Show Diff with Working Tree (the diff window takes a change, not two revisions), update and retry of a rejected push, View Commits on the deleted-branch notice.
- Not registered because nothing tests or completes them yet: `vcs.update` (needs the Update Project dialog), `vcs.showLog`, `vcs.resolveConflicts` (needs the conflicts dialog; a merge that stops on conflicts raises a notice instead), and the continue/abort actions of rebase, cherry-pick and revert, which the title-bar controls already carry out.
- Tests that run `git` beside the window retry when the repository's index is locked by the window's own read.

## D72. Bookmarks, Structure, TODO and Build tool windows

Four tool windows written by one subagent without a compiler (40 tests, no compile error) and wired by another, each in its own child module of the workbench with an end-to-end test.

- Bookmarks: the window works on a copy of the workbench's store and the two are copied into each other after every change on either side.
- Structure: follows the active editor only while the window is visible, reloads the outline half a second after edits settle, and follows the caret only with Autoscroll from Source on, which is off by default as in the component.
- TODO: the project scan is the Find window's search; open editors are scanned 400 ms after an edit. The preview is a read-only editor on the file. Not done: the Changes tab is empty (change lists are not fed), settings are not stored (the model has no stored form), nothing rescans on save.
- Build: a real driver over the backend's build. One test builds a plain project (the backend refuses it with its own message) and another runs real `cargo` builds of a tiny crate without dependencies: a type error listed with its location, a fixed build succeeding, a build stopped by the action ending cancelled. That test needs `cargo` on the PATH and up to a minute of real time; it passed ten runs in a row.
- An older test of Search Everywhere searched for "item" and counted rows; registering the two TODO actions, whose titles contain "Items", added rows, as it would in the webview. Its fixture word changed; its assertions did not.
- The workbench has no badge on a tool window's stripe button yet, so the TODO count and the Build error count are computed and tested but not shown.

## D73. File operations in the Project tool window

Creating, renaming, moving, copying and deleting files, cut/copy/paste, Copy Path and the tree's context menu are live (views by one subagent, driver by another, nine end-to-end tests on real temporary projects).

- The tree keeps its own rows and selection; the workbench owns the operations' state and copies the tree into it before every step and back after it. Rebasing the tree on the operations' state would have shown a placeholder row while a name is typed; the New popup has a name field instead, since the tree has no inline editor.
- Rename and Move run the backend's refactoring: references and text occurrences are searched (cancellable), a preview shows when other files change, the move is undoable, and open editors follow the file to its new path.
- No test deletes a file for real. The backend's delete sends the file to the system Trash and has no hook to redirect it, so a test would leave files in the owner's Trash. The question, the safe-delete usage search, the stored choice and the failure report are tested with a file that is already gone; the successful half (Trash, Local History, tree reload, closing the tab) is written and has never run.
- Not done: the tree's inline rename and New menu (no inline editor), putting files on the system clipboard, Mark Directory As (a warning says so), the project's source roots (so no "Path From Source Root" and no package rows).
- A hint at the caret does not exist yet; "Copied …" shows as a notification.

## D74. Committing, and the rest of the Git Log

A commit can be made from the native app: the changes tree has check boxes, the Commit window has the message area with its history, Commit, Amend, Commit and Push and the options; the Git window has the filter bar, the branches pane, the details of the selected commit and its changed files with their diff. Views by subagents without a compiler (59 tests, 8 compile fixes), wiring by another with five end-to-end tests on real repositories. The same slice compiled the two shared pieces many views wait for: a multi-line text area and tooltips.

- The message stays in the field after a commit unless `vcs.commit.clearInitialMessage` is on, as the component does; the test asserts both.
- Before-commit checks never run: there is no runner. Analyze Code and Check TODO are on by default and are shown ticked, but a commit goes through whatever they are set to, with a line in the log saying which checks were skipped and no claim that any passed. This is the largest functional gap of the slice.
- A file is committed whole: the diff has no per-block check boxes here.
- A refused commit (a failing hook) keeps the message and raises "Commit failed" with the backend's text; the box has no inline place for a backend failure.
- Commit and Rebase after Fixup is not carried out (nothing sets a fixup target).
- The log's filters reload the existing list through the model's query. "me" as an author is not resolved; the choose-many, date and folder choosers and the graph and view menus are not built.
- The branches pane's toolbar actions and Go to Hash/Branch/Tag are drawn and not registered: no test runs them.
- Tests wait for the commit's notification before asking `git status`: read earlier, the index lags HEAD for a moment.

## D75. The count is checked against what the crate declares, and rows the wiring already proved are recorded

- A hole in the model-only count (found by the advisor): the inventory took every file on disk under the app crate as part of it, and every file under the workbench as the application. Views written ahead of the build sit on disk undeclared, so three rows were counted as reached that were not. The inventory now follows the crate's `mod` declarations from `lib.rs` and `main.rs`: a file no `mod` line names is not compiled and counts for nothing.
- `inventory.mjs --dry` counts without building (it skips the check that cited tests exist and writes nothing), for use while an agent holds the build.
- Rows of backend commands, overlays and title-bar items had not been recorded since the Run slice, although the end-to-end tests of branches, commit, project files, build, diff and run configurations reach them. 45 command rows and 8 overlay and title-bar rows are recorded now: at parity where a named test goes through the call, partial with the reason where the call is written and no test reaches it.
- 43 files written ahead of the build are committed in one commit that says they are not compiled. They were on disk only.
- Outside the app crate, the branch changes five files of two backend crates and the root manifest since the base commit (a snapshot accessor for documents, highlight queries for grammars, the workspace's exclusion of the app crate); `src/` and `src-tauri/` are untouched (measured with `git diff --stat a950eeca -- src src-tauri crates Cargo.toml Cargo.lock`). The Tauri app has not been rebuilt or retested since those early changes.

## D76. Code intelligence in the editor

Completion, go to declaration / implementation / type declaration (by action and by ⌘/Ctrl+click), hover with problems, Quick Documentation, parameter info, the intentions list and underlined diagnostics work in the editor (popups by one subagent, wiring by another).

- One controller per editor, held by the editor as its overlay and holding the editor weakly: it lives exactly as long as the editor, and the workbench keeps no map of them. The lookup and parameter info never take the keyboard: they claim their keys on the editor's focus. Escape closes parameter info, then the lookup, then hover and hint, then the Find bar, then the extra carets.
- The editor now reports every edit with its origin (typed text, deletion, completion, history, external) and caret moves, lets an owner take a pointer press, and draws an owner's overlay.
- The tests use a real language server process, not scripted answers: a user language file in the test's own config folder points `*.mock` files at a small `python3` server over stdio that answers from a JSON file the test rewrites. So the backend's LSP client, the real operations and the popups are all in the path. The tests need `python3`, as one of the backend's own tests already does. What a real server does differently (incomplete lists, resolve data, cancel timing, streamed implementations) is not proven.
- In Light mode, which is the default, no server starts, so typing shows no lookup: the component offers word completion there and this app does not yet. Go to Declaration without a server finds only names local to the file.
- Not wired: Tab through a snippet's fields (no template session in the editor), live templates and contributed rows in the lookup, Tab between arguments in parameter info, the bulb and the IDE's own intentions, the quick-fix and links inside the hover, the ⌘-hover declaration preview, next/previous error.
- The settings and code-insight wiring were drafted by writers without the build (the advisor's suggestion to shorten the build lane); they are committed undeclared and wait for integration.

## D77. The first launch of the real binary since the wiring slices

Every slice since the Run tool window was proven in the test harness only. On 2026-11-13 the debug binary was built and started on a temporary git project with its directories redirected (`OPENIDEA_GPUI_HOME=<temp> target/debug/openidea-gpui <project>`), left for 25 seconds and stopped by its process id.

- Measured: the process stayed alive; the backend logged the project as opened and indexed its two files; standard error was empty and standard output held no panic and no error, only two warnings from GPUI's text system about duplicate system fonts. At 25 seconds the debug build used 121 MB of resident memory and 0.1 % CPU.
- Not done: nobody looked at the window (a screenshot would have captured the owner's screen), no key was pressed and no action was run in it. So this proves start-up, opening a project and idling without a crash, and nothing about what is drawn or about the macOS menu's key path.
- The run uses its own application identifier and directories, so it does not touch the installed app's data or its single-instance state.

## D78. Settings, Problems, Find Usages, Hierarchy; and wiring drafted before the build

- The Settings dialog is live: the page tree with search, staging with Apply / OK / Cancel, the Global and project scopes, the built-in pages made of plain options (170 options that write a setting), and the Keymap page with presets, search by name and shortcut, and assigning shortcuts with conflict warnings. Pages that are views of their own in the webview (fonts, colour scheme, code style schemes, file types, the language pages, inspections, SSH and others) show "This page is not available yet". The stored keymap settings are applied to the running keymap at start and on change; nothing applied them before.
- Problems, Find Usages (tool window and popup) and Hierarchy are live. Without a language server the backend itself answers Find Usages with text matches and says so in the note; Hierarchy answers that no server is available. The semantic paths are not exercised by any test.
- A defect in a draft, caught by the integrating agent: the Plugins page drew a row with a plain text field for every secret a plugin declares, although its notes said no secret field would appear. A text field cannot mask its content, so a secret would have been typed in clear. The rows are now drawn only after the secrets were read, nothing reads them, and a test fails if that is undone. Setting a plugin's secret is not wired until a masked field is used.
- The Plugins page's listing through the plugin host compiles and has never run: a workbench test cannot start the plugin host, which sets process-wide state.
- Wiring written before the build works: writers without the compiler drafted the workbench module, the lines for `mod.rs` and the end-to-end tests for these two features, and the integrating agent took about 30 minutes for code insight and 50 for Settings, where earlier slices of this size took 45 to 80 each. The cost: the drafts' notes state things as facts that were guesses, and one was the defect above. Notes are read as hypotheses.
- One older test fails about once in fifteen runs of the whole suite (the branch widget's checkout test) and passes alone; the cause is not found yet.

## D79. Merge conflicts, the status and title bars as hosts, tooltips

- A merge or rebase that stops on conflicts now opens the Conflicts dialog: Accept Yours / Accept Theirs write and stage a side, Merge… opens the three-way merge editor (accept and ignore per change, the non-conflicting commands, typing in the result, Apply writing the resolved text and staging it), and a rebase whose last conflict is resolved continues by itself. A sticky notice names the conflicted files, and an editor of a conflicted file shows a banner. The merge editor is a front window in its own layer: the editor area cannot hold it as a tab yet. A file with CRLF line breaks is written back with CRLF (tested byte for byte).
- The 30 merge-editor actions are not in the generated action manifest, so they are registered with their metadata by hand; ids, titles and conditions were compared with the webview's table one by one.
- The backend names the merge commit after the full reference (`Merge branch 'refs/heads/feature'`), because the branch menu passes the full reference, as the webview's does.
- The status bar and the title bar are hosts of registered items with priorities, visibility stored in the settings and a context menu; the title bar has the Customize Toolbar dialog. What a user sees differently in a project window, following `TitleBar.tsx`: the window title is no longer drawn in the full bar; on the left a project widget, then the branch widget only in a repository; on the right the run widget, the mode switch, a Search Everywhere button and a gear with its menu. With the main toolbar off, the compact header shows the title. The status bar gained the language services item, indentation and the read-only lock, and the tasks item shows a progress bar.
- Tooltips exist now and were added where the webview has them on views already built: the Run, Services, Build, Commit and diff toolbars, the tool window stripe buttons, a tool window's Hide, the tab close button; each family has a hover test.
- The intermittent branch test was a wait on the wrong thing: an operation refreshes, then notifies, and the backend's own file event can update the widget before the operation's read returns, so a test that waited for the widget and then asserted the notice could run early. The tests wait for the notice now; assertions are unchanged. The same class was found in three other tests and fixed the same way.
- Not wired: the in-editor accept / next conflict actions on marker blocks, blame in the merge panes, the indent menu, the read-only toggle, the Background Tasks popup, the gear's Theme and Keymap submenus, and the status items that have no data yet (memory, Vim, breadcrumbs, agents, remote, language level, plugin items).

## D80. Editor tabs that hold a viewer; the Database tool window; a shortcut gap in the action manifest

- An editor tab can hold a view other than a text editor: a second field beside the editors, keyed by tab, with the kinds `image` and `binary`. Neither is restored with the session, as in the webview. A Markdown file stays a text tab and gets its Editor / Split / Preview buttons and a live preview. Open as Text adds a read-only text tab beside the hex tab (`controller.ts` admits a new input and closes nothing).
- Which viewer a file gets is decided by the same backend call that opens its document. A separate probe before the open was drafted and was blamed for two intermittent completion tests; it was replaced by one call. Correction (2026-11-13): that did not remove the intermittency. `editor::intelligence::tests::typing_completes_from_the_server_and_enter_inserts_the_item_with_its_import` still failed in 2 of 30 runs of its module alone (the count of completion requests after typing `obj.`), in a module that does not go through the workbench at all, so the probe was not its cause. A failed probe falls back to the text editor, as `resolveFileEditor` does.
- The Database tool window uses SQLite on a temp file in its tests: no server, no network. The table editor and the results live in the lower half of the Database window, not in an editor tab or a Services view; those rows stay partial.
- Secrets: a typed password or SSH secret travels in a type whose `Debug` prints `<redacted>`, the password field paints and hands to the input method only bullets, Copy and Cut leave the clipboard alone, and a password inside a URL is redacted in every `Debug` of the form. Tests prove each. Known and not fixed: `DataSource` in `crates/ide-db` derives `Debug` with its URL, which in URL-only mode holds `user:password@`; nothing prints it today, and the fix belongs in that crate.
- `assets/gen/actions.json` misses the `when` / `keyWhen` an action gets through a helper in the webview source. The Database tree's shortcuts (F4, Delete, ⌘R, ⌘D, ⌘B…) therefore ran in every window until they were tied to a new `databaseViewFocus` context key. Every later slice checks each shortcut it registers against the webview's condition, with a test that the key does nothing elsewhere.
- `inventory.mjs --listed <dir>` reads saved `cargo test -- --list` outputs, so the inventory can be regenerated while the build is busy.
- Not wired: the console's own toolbar, choosing a source for a plain `.sql` file when there are several, Query History, the Query Plan tab, schema-object dialogs, SSH configurations, import and export, 17 `db.*` actions; in the viewers, the preview's code colouring and font zoom, renaming a file under an image or hex tab, plugin editor wrappers.

## D81. Agents and the debugger; a stricter bar for some rows; the manifest generator

- The Agents tool window and the debugger are in the application, each tested end to end against a fake peer: an ACP agent and a debug adapter that are `python3` scripts declared in the test's own config folder (an agent file; a user language file with a `[debugger]`). No real agent or debugger starts in a test.
- A conversation stays docked in the Agents window even when the placement setting says Editor: making it an editor tab needs the tab builder to reach the agents' state, which it cannot yet. The placement actions run and are recorded as partial.
- Debug shows breakpoints (dot, hollow when inactive) and the execution line as editor marks. `F7`/`F8`/`F9` and the other stepping keys act only while a session is paused, `F4` and `F2` only in the Frames and Variables panes, `⌘R` only in the Debug window, as the webview's conditions say; a test proves each.
- The Run widget's Debug button is always drawn, as `RunWidget.tsx` draws it; an earlier test that expected it hidden without a debugger was wrong and was changed.
- Secrets: the agents' store and the built-in agents list print the names of an agent's environment variables in `Debug` output, never the values. The same derive in `crates/ide-agent` and the URL in `crates/ide-db`'s `DataSource` are fixed in the backend crates in the next slice.
- Known backend behaviour, shared with the webview: `ide-dap` emits a synthetic `Continued` after a step or resume is answered, so a `stopped` that arrives before it is overwritten and the session shows Running while the program is paused. A real program takes longer than that window; the fake adapter waits 0.2 s before it reports a stop.
- A stricter bar, applied to existing rows after a sweep of every unticked row against the tests: a boolean context key is at parity only when a test reads it on and off; an action only when a test runs it through the registry or its key; an IPC row only when an asserted state can only come from that operation. Eleven rows that had been ticked on less were downgraded (`terminalFocus`, `vcsAvailable`, `diffEditorFocus`, `projectOpen`, `editorFocus`, `editorTextFocus`, `mergeEditorFocus`, `imageViewerFocus`, the `run` and `stop` actions, `db_connect`, `db_connected`), and 53 rows that existing tests already proved were recorded. The sweep's lists are in `notes/sweep/`.
- The action manifest generator (`gen-data.mjs`) now follows an action into a helper that spreads defaults over it and into a `.map` that adds its `run`, resolves conditions built from template strings, `+` and local constants, and fails on a condition it cannot resolve unless it is on a short allow-list. 39 actions gained their conditions and 18 GitHub actions appeared. No shortcut or enablement of an action registered natively changed. The inventory's own action list still requires a literal `run` and so omits those 18 GitHub actions; it is corrected with the GitHub slice.
- Not wired: for agents, the New Task page, Find in Conversation, the status item, queued messages, background tasks, the changes pane, `addAgent`, `regenerateReply`, `forkReply`; for the debugger, attach, hot swap, smart step into, exception breakpoints, the console's links and find, the Stop menu's debug sessions.

## D82. Closing, quitting and opening a project; completion after a pause; what the Tauri side was checked with

- Closing a window asks in the order of `closeGuard.ts`: the exit confirmation (last window, no project, on by default, with "Don't ask again"), running processes, unsaved changes (Save All / Don't Save / Cancel); then it stops the processes and closes. The question is asked whenever something is unsaved, whatever autosave says: an earlier draft saved silently when autosave was on, which the specification contradicts. Autosave on window blur is held back while a close is being decided, and nothing is saved after "Don't Save".
- Quit confirms the exit once, then each window decides its own close; one window that stays calls the quit off. The earlier handler asked every window from inside the active window's own update, where that window cannot be reached, so its unsaved changes would not have been asked about; the loop now runs outside the update, with a test.
- Closing the last project window shows the Welcome screen instead of closing the window, as the webview does.
- Typing starts completion only after a 100 ms pause, as CodeMirror's `activateOnTypingDelay` does. Before, every keystroke asked the server and cancelled the previous request, which is what made a completion test intermittent (three requests reached the server once it had attached). Measured after the fix: 40 runs of the module without a failure. Still different: with a request in flight, the next keystroke re-asks at once and cancels it; the webview lets the running request answer first.
- Focus Editor (Escape in a tool window) gives the keyboard to the active tab's view when that is an image or hex view; before, the keyboard was left nowhere.
- Backend crates: `DataSource` (`ide-db`) prints its URL with the password redacted, `AgentProvider` and `CustomAgent` (`ide-agent`) print the names of their environment variables only. Serde and the public API are unchanged. The agents' settings page shows the values of an agent's environment in clear, as the webview's does (`BuiltinAgents.tsx`).
- The Tauri side, run from the repository root after those changes: `cargo test -p ide-language -p ide-workspace -p ide-db -p ide-agent` (all passed: ide_agent 178, ide_db 75 plus its SQLite 13 and others, ide_language 107, ide_workspace 127 plus 50 in three integration suites), `cargo check -p openidea` (finished, no warnings; the frontend bundle was not needed), `cargo clippy -p ide-db -p ide-agent --all-targets -- -D warnings` (clean). Not run: the full workspace test suite, the frontend's `pnpm` checks, a build of the Tauri bundle.
- The plugin test that needed an environment variable is a normal test now: it copies a checked-in fixture plugin into its temp home and runs a command of it. Fixtures that declare a secret are avoided there, because the plugin host installs the login keychain as its secret store.
- Not wired: a live debug session is neither named nor stopped when a window closes; there is no Terminating panel with Kill (the close stops again after 10 s); document windows, Attach, remote projects.

## D83. Git operations, shelf, change lists, patches, commit checks and the log's actions

- The repository operations (Pull, Update Project, Merge, Rebase, Reset HEAD, New Tag, Stash, Unstash, Rollback, Add to VCS, the ignore actions, Remotes, Worktrees, the Operations popup, the continue/abort/skip of a stopped operation), the Shelf tab with change lists and patches, the before-commit checks and the Git log's commit, branches-pane and changed-files actions are in the application, each run through the registry by a test on a temp repository with a local bare remote. 89 action rows are recorded from them, 16 as partial. The missing time on shelf rows is counted against Show Shelf and Shelve only; Refresh, Unshelve, Rename and Delete are recorded done.
- Rollback with the keyboard in an editor rolls back the changed lines under the caret, as the webview does; elsewhere it rolls back files.
- A password inside a remote URL is never painted: the Remotes list, Pull's remote choice and Edit Remote show `user:***`. Leaving the mask in the edited URL keeps the stored password. The webview shows the URL as stored, so `vcs.manageRemotes` stays partial on purpose. Still open: the form's own row types derive `Debug` and can carry a URL being typed.
- No repository read starts while a Git operation of the window runs, and an operation waits for a read in progress. Measured by the integration run, not repeated afterwards: before, 3 failures in 150 parallel runs of two tests, one with `index.lock: File exists` in the Git console; after: 0 in 150 and 0 in 400. Operations that go through the plain runner (stash, rollback, unshelve, apply patch, merge and rebase from the dialogs) are not covered by this, and the backend's own index write-back is not serialised with the Git commands. Hypothesis, not confirmed: the lock holder is the status refresh writing the index back (`crates/ide-vcs/src/status.rs`).
- The Commit tree follows working-tree edits (the backend raises its version control event only for changes under `.git`; the integration run measured the tree stale for 30 s before) and its checkboxes follow the change lists: only the active list is included by default.
- Commit waits up to 2 s for the language servers to analyse the files when Analyze code is on, as `commitChecks.tsx` does; a file a server has reported clean does not wait. Three commit tests take 2 to 5.5 s each because of it; the suite takes about 25 s instead of 18.
- Revert Selected Changes in the log's changed files accepts the headers of a reversed patch (`b/… a/…`). The webview's own pattern looks like it has the same defect (read from the source, not run).
- Tests are isolated from the user's Git configuration by per-repository settings. Measured by the integration run with a hostile global config (signing, autocrlf, hooks path, `pull.rebase`, `merge.ff=false`, `diff3`, `noprefix`, `rerere`, `showUntrackedFiles=no`): the suite passes. Not isolated: a global `core.excludesFile` that matches the tests' files makes 4 tests fail although the repository overrides it. Hypothesis: `crates/ide-vfs/src/rules.rs` adds the global ignore file with `build_global()`, which reads the user's Git configuration whatever the repository says; to confirm, run one of the four tests with that line disabled. Either way the tests read (never write) the user's global ignore file through the backend.
- Found in the backend and left as is: patch creation breaks under `diff.noprefix` (prefixes are not pinned).
- The title bar shows the "Version Control" fallback with a Show Git Log row now that the action exists; the test that expected no fallback was changed by the integration run on its reading of `TitleBar.tsx`, which was not re-read afterwards.
- Not wired: `vcs.compareWithBranch`, `vcs.log.openExternal`, `vcs.annotate`, `vcs.showHistory`, `vcs.localHistory`, the log's toolbar, multi-selection in the log (the many-commit menu is reachable only with an argument), the "Changes between" list, the collapsed graph, a filtered log for a branch or path, times on shelf rows, the Reformat / Optimize imports / Cleanup checks (drawn disabled with a reason), links on TODO hits, a multi-line tag message.

## D84. Proof tests, the fixes they demanded, and three smoke tests

- 82 more drafted proof tests are in the build (87 in all with the five of the Git slice), with seven new smoke tests in three files: backend events caused for real (an edit and a save made by the backend, the project's settings file written on disk, a file created in an expanded ignored folder, data sources saved by the backend, a file changed on disk under unsaved edits), every Settings page opened and one control changed with the mouse, and Run All / Restart All / Stop All of the Services window on plain local processes.
- Where a drafted test showed the native behaviour to differ from the webview, the application was changed, not the test: a quick fix's submenu items run their command; Close All Tabs closes the tabs of every pane and clears the reopen history; usages reach the results view as `{symbol, locations}`; `focusedDirectory` is published while the Project view has the keyboard; the run console's find field keeps the IDE keymap out while it still gets every key; Copy Reference is an action of its own; syntax highlights move with an edit; the navigation modifier follows key presses and the window losing focus; alt-drag selects a block of columns.
- Settings: the smoke test changes one control on each of 38 pages and reads the stored value after Apply. A later read of the pages against the webview's sources found only 8 of the 18 compared pages with all their controls drawn (the debugger, stepping, Python debugger, run, language, mode, agents chat and shelf pages); on the others a control of the page's own kind still says "This page is not available yet", and they are recorded as partial. The integration run had reported all 38 as complete. About 45 pages are not built and say so on the page ("This page is not available yet"): the ten colour scheme pages, inspections, file and live templates, spelling, files, TODO, Vim, project trust, plugins, the version control root page with mappings, issue navigation, log and GitHub, project structure, CMake, Gradle, Maven, run targets, deployment, fifteen language pages, agents, actions on save, external tools, SSH configurations, date formats, HTTP proxy, postfix completion. The Keymap page is its own view and was not exercised.
- Left out of the build, with their files kept undeclared: the drafted quit test (it skipped the exit confirmation and repeated an existing test) and eleven highlighting tests whose fixes belong in the highlight queries of `crates/ide-language` or in `languages/cmake.toml`. Natively, CMake argument keywords, numbers, predefined variables, `${…}` expansions and function definitions are not coloured as the webview colours them, shell expansion delimiters are not marked, and template code in Kotlin, Scala and Dart is almost uncoloured.
- The generated action manifest gives `find.popup.nextResult` the title "Recent Searches": actions made by a factory get the wrong primary title from the generator. Not fixed yet.
- During this integration the integrator ran `cargo fmt` over the crate, which rewrote drafted files of another agent, and then restored seven of them from the last commit, losing that agent's uncommitted edits; the edits were re-applied from the agent's own copies and committed. Integrators are told since to format only the files they edit.
- The Tauri side after this slice: `cargo test --workspace --no-fail-fast` from the repository root exits 0 (3,325 passed, 0 failed, 62 ignored, 142 test binaries). The frontend sources are byte-identical to the base commit, so its checks were not re-run.

## D85. Plugins in the application

- The Plugins window and Welcome pages, plugin commands as actions, plugin prompts and notifications, log tabs, background tasks with a Background Tasks popup, run and debug requests of a plugin, gutter actions, plugin menu items, a plugin's settings page and the events a plugin listens to are in the application, tested with the fixture plugins checked into the repository, copied into each test's temp home.
- The macOS login keychain store of the plugin host is compiled only outside test builds; in a test the host keeps secrets in a file under the temp home. The integration run measured the test binary with `nm`: no symbol of the keychain store or of `security_framework::passwords`. The binary still imports keychain functions from another dependency (hypothesis: the SSH keyring of `crates/ide-ssh`); whether any test can reach them is being audited separately.
- A secret typed into a plugin's secret field is never painted (bullets only), copied, logged or printed by `Debug`. One log line that printed a whole plugin event was reduced to the event's kind.
- A plugin's debug request starts the real debugger through `debug.pluginStart`, as the webview does, and the plugin hears the exit code. The Java (JDWP) branch is not built.
- Plugin commands run on the backend's blocking pool, as the Tauri adapter runs them; on the window's own executor a long command kept the window from hearing that a task had started.
- The plugin hosts of tests are locked process-wide, because the host's editor providers, run detection and syntax are process-wide and parallel tests answered each other's windows. The suite takes about 50 s instead of 30.
- The webview has no "link a plugin folder" action; what it offers for a local folder is Install Plugin from Disk, which asks for consent with the high-risk permissions first. That is what was built and tested.
- A click on a plugin's gutter icon neither moves the caret nor toggles a breakpoint. The integration run checked the test by removing the three guards: it then fails with an extra breakpoint.
- The Git forms' row types print `<redacted>` for their values in `Debug`, so a remote URL being typed cannot reach a log through them.
- Not wired, so a plugin that relies on them shows nothing natively: plugin tool windows and views (`contributes.views`, view changes and reveal requests go to a log line), opening a terminal from a plugin, run targets, Run Anything providers, `plugins.updateAll`, Find in a log tab, `run.pluginStart`, the IntelliJ imports, plugin items in the editor and tab menus (no native host until the context menus land). Correction (2026-11-13): an earlier version of this entry said the Bazel plugin's tool window could not appear. Its manifest (`plugin.toml`, read, not loaded) contributes no view: it contributes nine commands, a gutter-actions provider for Java, items in the Tools, editor and Project view menus, a settings schema, three languages and four run detectors, one of them with `debugger = "java-debug"`. What the native app lacks for it is the Java (JDWP) branch of a plugin's debug request, so Debug Target and Debug Test File cannot attach; the rest has a native handler. Nothing of this was exercised with the Bazel plugin itself.

## D86. Context menus; the manifest resolves actions made from a list

- The editor, gutter, editor tab and Search Results context menus, the plugin menu slots, the tool window Options menu and the stripe button menu are in the application. A row is shown only when its action is registered, as the webview's menu does, so each menu grows as later slices register actions; the tests assert the literal row lists and must be extended with them.
- A right click in the editor text moves the caret unless it lands inside the selection, which it keeps. A right click in the gutter sets no breakpoint: the test proves that the right press emits no gutter press while a left press at the same place does.
- A press beside an open menu closes the menu and is swallowed, and only the left button closes it. The webview closes on any button and lets the press through. Left as is for now because the backdrop is shared by every popup; recorded as a difference.
- Not built: the Problems menu and the Project view's group rows have no test; the `mainToolbar` and `commit.message` plugin slots are untested; the gutter's Soft-Wrap row (no soft wrap), and the Search Results rows Exclude / Include, Replace Selected and Preview Replacement are missing; twelve menu groups have no rows because the feature state they need does not exist natively.
- The action manifest generator evaluated an action literal built inside `LIST.map(([id, title, …]) => ({…}))` once, without binding the callback's parameters, so every element got the same record (the four Find popup actions were all titled "Recent Searches"). It now produces one action per element with its own title, icon and condition. 64 ids that were counted as "built at run time" are listed by id, and `dynamic` went from 20 entries to 11. The inventory drops the "built at run time" row of a place the manifest now lists by id, so no action is counted twice; it has 3,145 rows.
- Hypothesis from the integration run, not checked: action titles built from a template (19 Project view ids) are translated with the plain lookup, not the template lookup, and may show in English in the Chinese UI.

## D87. Editor core and language actions; secrets in files under an isolated home

- Folding, extend and shrink selection, auto-indent, scroll to center, font size, back and forward, last and next edit location, copy path, reload from disk, read-only, paste from history; rename by dialog, reformat (also with a named formatter), optimize imports, cleanup, generate / implement / override, apply quick fix, next and previous error with its description, usages highlighting, quick definition, go to test, next and previous method are in the application, each run through the registry or its key by a test; the language ones against the fake language server.
- Folding keeps a row map, and every conversion between a document line and a painted row goes through it. One test folds a region and then proves, with the fold in place: clicks land on the right line, the caret skips the hidden lines, paging accounts for them, a breakpoint mark, the execution line, an error underline and a plugin's gutter icons are painted in the row of their own line, typing above keeps the fold, and scrolling to the end shows the last line.
- Not built: fold chevrons in the gutter, saved folds, Expand to Level, soft wrap, unwrap, in-place rename (rename is a dialog), the reformat dialog, Refactor This and the extract / inline / move refactorings, super method. Back and forward record only switches between files.
- For nine language shortcuts (⌃I, ⌃O, ⌥Space, ⌘Y, ⇧⌘T, ⇧F6, ⇧F2, ⌃⇧↑, ⌃⌥↑) a test proves the key does nothing while a tool window has the keyboard, but no test presses the key in the editor, so the positive half is unproven.
- Secrets: with an isolated home (`OPENIDEA_GPUI_HOME`, and every test) the backend keeps saved passwords (SSH, database, proxy, Git) in an owner-only file next to its index, and the OS credential store refuses to work for the rest of the process; the plugin host does not install its keychain store there either. The Tauri app never turns the switch on, so its behaviour is unchanged, and its generated bindings are unchanged. Proven by unit tests in `crates/ide-ssh` and by a native test that saves an SSH and a database password through the calls the UI makes and reads them back from the file. Not measured: the plugin half in a real run (it is compiled out of test builds).
- A test that starts an agent task without selecting the fake agent first would start the real `claude` CLI on the machine, because the draft's default agent is `claude`. All seven tests that start a task select the fake agent first (the integration run checked by script). A guard in test builds is owed.
- One test, `the_merge_editor_opened_from_the_banner_runs_its_actions_and_the_rebase_is_continued_from_the_notice`, failed once in the integration run's first five whole-suite runs (a 30-second wait ran out; that run took 87 s instead of 52) and passed in every run after, including five of mine. Cause not found. Two hypotheses, neither checked: the index-lock race of D83 on the path left uncovered there, or a file-watcher event that never arrives under load (with three test binaries running at once, the integration run saw four other Git tests fail on waits for the watcher).

## D88. Git commands and repository reads never overlap; the intermittent merge test; a guard against real agents

- The test that failed once in thirteen suite runs (D87) was reproduced by running the test binary in 3, 6 and 10 processes at once: 70 failures in 110 process-rounds before the changes, each with the window's Git console, operation, read queue and event trace printed by the wait that ran out. None was starvation: in every one the awaited state was still absent after a further long poll.
- 41 of the 70 were the test itself: Reload in the merge editor returns at once and the decision the test made next was dropped when the reload landed, so Cancel closed without asking. The webview behaves the same way; the test now waits for the reload. This is the most likely cause of the original failure, though that failure's wait label was never recorded.
- 28 were a real race in the application: a Git command (stash, rollback, a rebase or merge from the dialogs…) ran while a repository read of the window was in flight, and Git failed with `index.lock: File exists`, because the backend's status read writes the index back. Every Git command of a window now waits for a read in flight and for any other command, and holds reads off until it ends; a read asked for meanwhile starts afterwards. The earlier fix (D83) covered only some paths and dropped events while a remote operation ran. After: 0 failures in 245 process-rounds of the same stress, and 0 in 50 more with the merge proofs. (The commit message of `9c26a6e2` gives 105 and 325 for these two totals; the numbers here are the ones in the integration run's tables.)
- Consequences to know: Commit or Stage during a long fetch or pull now waits for it. An external `git` run by the user can still meet the backend's index write-back; the tests' own `git` calls retry on the lock. No lost file-watcher event was seen in any trace.
- Open: the gate is opened by a call after the command, not by a guard, so a command whose task were dropped mid-way would leave the gate closed. The next integration replaces it with a guard and a test.
- In test builds the agents code refuses to start any agent that is not a custom agent declared in the test's own config folder under the temp directory, and a test proves that a task with the default agent (`claude`) is refused and starts nothing.
- The merge editor's actions are proven through the registry on real conflicted repositories (25 rows). Resolve Merge Change now takes its `{indexes, action}` argument. Diff Settings and Jump to Source act on the merge editor that has the keyboard; Jump to Source asks the Cancel Merge question first when the result was changed, because natively the merge editor is an overlay and leaving it would drop the work (in the webview it is a tab that stays behind). `merge.annotate` stays partial: the side panes draw no blame.
- One test lists every action registered in a project window (491) and requires each to be in the manifest or to match one of its run-time id patterns; none is unknown. Of those, 198 have an action row that is not yet at parity (`notes/sweep/registered_not_done.md`).
- The editor no longer records where it painted its marks outside test builds.

## D89. Project trust and Restricted Mode; application actions; the Git gate is a guard

- An undecided project asks "Trust Project?" in the window it opens in, at the start of every opening, as the webview does: Trust, open in Restricted Mode, or Don't Open (a cancelled opening). New Window, Open in New Window and a folder given on the command line ask in the new window. The decision is stored in the app's data folder. Tests that go through the open flow trust their temp folder first through the same store write.
- In Restricted Mode nothing of the project runs, each proven by a test with a trusted control: no database connection, schema read or Test Connection (the native code had no check at all; it now asks the backend's authorisation with each database command's name, as the Tauri adapters do), no run configuration, no debugger, no language server, no project plugin (they are unloaded when trust is revoked), no Git hook, no agent. Known gap, by reading: saving run configurations and the Git file writes (patches, shelf, change lists) are not trust-checked natively.
- Window layouts (store, restore, named layouts), Zen mode, About, New Project, Invalidate Caches (removes exactly the cache folders under the app's home; a test plants files inside and outside), settings export and import (a malformed file is refused and changes nothing), power save mode and Restart are in the application. Restart goes through a relaunch seam and a test build cannot relaunch. Not checked: whether a relaunch on macOS keeps `OPENIDEA_GPUI_HOME`; until it is, Restart must not be used in a run with an isolated home.
- Found and fixed on the way: Zen mode panicked by reading the window it was updating; a long parent folder pushed the trust dialog's buttons out of the window; with a restricted project open the Plugins page re-listed without end.
- The Git gate of D88 is released by a guard: a command whose task is dropped opens the gate on the next turn, and a late release never opens a later command's gate. Hypothesis from the integration run, not checked: the separate "another operation is in progress" flag of remote operations can stay set if such a task is dropped.
- Quit also asks about a program running under the debugger and stops it, as the webview does. A run console in terminal mode is told its size and follows a resize; before, it was never told, and its output stopped reaching the view after the first paint. Tooltips hide on any key press and when the window loses focus.

## D90. Project view, terminal and Services actions; a trash seam in the backend; the editor hint

- Select In, File Path, Reveal in Finder, the copy path forms, Mark Directory as, the Project view's options and sort orders, cut and paste, delete with Undo; Open in Terminal, Run Command, predefined sessions, splits and Close Session; Move Up / Move Down in Services and the Docker view options are in the application, each run through the registry or its key by a test.
- The backend's delete takes a per-host trash override (`Host::set_trash`, unset by default; the Tauri app never sets it, its bindings are unchanged). Tests point it at a folder inside their temp home, so a confirmed delete is proven for real (the file leaves the tree and the disk, Undo brings it back from Local History) without the system Trash. Reveal in Finder goes through a seam that records the request in tests.
- Close Session asks Terminate / Cancel before ending a running command, as the webview does; ⌘W and a tab's × still close without asking. Terminal tests of this slice run a plain `/bin/sh` script, not a login or interactive shell. The older terminal tests still start a login shell with the temp `HOME` and no history file.
- Docker: the machine has a `docker` program installed, so the Services tests written before this slice could have reached it (by reading; what they did was not measured). Every workbench test now writes a Docker configuration with no connections. No Docker operation is built.
- The editor hint (Copy Path, Add Caret at Next Occurrence, Cyclic Expand Word, Go to Line's and Complete's messages) shows at the caret, as the webview shows it; before it was a notification, or only a log line. The Background Tasks action only opens its popup and the status-bar item's click toggles it.
- Not built: Find in a terminal, the Project view's view modes, six Project view options, Unmark for a marked directory, inline rename, ⌘Z in the tree, files on the system clipboard for cut and paste, the folder column of File Path, contributed targets in Select In.
- A backend test of the repository that this work did not touch (`remote_plugins::api14_stage3_remote_terminal_pins_and_exit_events_stay_on_the_execution_host`) failed 3 times in 5 for the integration run and passed 10 times in 10 when I ran it alone. It reads a file as soon as it exists, while a shell redirect is still writing it. Recorded as an intermittent test of the base, not caused by the trash seam (hypothesis from reading the test; not bisected).

## D91. GitHub pull requests; three guards in test builds

- The Pull Requests tool window is in the application: it connects through `gh`, lists and filters pull requests, opens one in a tab with its branches, checks and changed files, refreshes, checks a pull request's branch out, opens its page and copies its URL, and changes repository. Ten actions are registered; create pull request, the timeline, review mode, comments, submitting a review, marking files viewed, open in a worktree, deleting a merged branch and clone are not, so their rows do not show.
- The backend's `gh` functions go through a `GhCli` value. The free functions the Tauri adapters call delegate to the located program, so the Tauri app is unchanged; the native app passes its own. Every native test gives a stand-in script in its temp folder that answers only the calls the test needs from canned JSON, unsets token variables and fails loudly on anything else; a test build asserts that the machine's `gh` is never the program run. Test repositories allow no network transport (measured by the integration run: an https fetch fails with "transport 'https' not allowed").
- "Run gh auth login" opens a Terminal tab with that command in production, as the webview does; in a test build the request is only recorded.
- A remote URL carrying a credential is masked before it is kept, shown, logged, notified or copied; a test formats the window's state with a sentinel token and finds none.
- Restricted Mode refuses `gh`: the native call asks the backend's authorisation with the command's name, which is how the Tauri app refuses it (centrally, not in the GitHub adapters). In the Tauri app the remotes are not listed either; natively they are listed before `gh` is refused, with the same result for the user (hypothesis from the integration run, by reading).
- A pull request checkout holds the window's Git gate and its busy mark like any other Git command.
- Three guards now hold in every test build: a terminal refuses to start a login or interactive shell or `gh` (five older tests did start a login shell with the temp home and were changed); a remote operation whose task is dropped no longer leaves the window answering "another operation is in progress" (a test failed before the fix); every delete goes to a trash folder under the test home, set when the backend starts, and a delete with no such folder panics.

## D92. Plugin views as tool windows; the Java branch of a plugin's debug request

- A plugin's contributed views appear as tool windows with the plugin's title and icon, on the side the manifest names; the plugin is not run before its window is opened. Message, tree, list, form and table bodies are drawn; node events reach the plugin; a refresh asked by plugin code repaints; reveal shows and focuses; disabling the plugin or closing the project removes the window, its panel, its stripe button, its action and its context key. A render failure shows "The plugin could not show this view" with the plugin's error.
- Plugin output is treated as untrusted: a tree is drawn up to 20,000 items and a table up to 1,398,100 rows however many the plugin claims, text is drawn as text.
- An HTML or web body is not drawn. The window shows the plugin runtime's own sentence for a body kind it does not know ("This view needs a newer version of OpenIDEA (html)."), which is the nearest existing text but is not what the webview does: the webview draws the HTML. Recorded as a known difference; post-message, views in the Services window, detail panes, plugin text catalogs and custom editors are not built either.
- A plugin's debug request for a run detector whose debugger is the Java one starts the process with the detector's debug arguments, reads the JDWP port from the line the JVM prints, and attaches the Java debugger. The port must be a number from 1 to 65535 and the debugger attaches to the loopback address only, whatever host the line names; both are tested. Following the webview: there is no time-out or message for a port that never appears (the process simply ends as a run), and a refused attach is reported as "Cannot start debugger for {name}" without stopping the process. Kept native: a run that does not start tells the plugin so; the webview reports success and the plugin never hears an end.
- In tests the process is a shell script that prints the JDWP line, the adapter is the fake one behind a loopback listener the test opens, and the port of the Java debug server comes from a test-only value; no JVM, Bazel or Java language server starts. The real Java flow's failure reports are therefore not exercised.
- Found: a trusted project's own plugins do not load when the project opens unless the Plugins page is shown, and the window's plugin listing leaves project plugins out. Being fixed in the next integration.
- Fixed on the way: in a plugin view's tree, Enter and Tab were taken as speed-search text, so Enter never activated a row.

## D93. The own controls of twelve Settings pages; the editor follows the font settings; a project's own plugins

- Twelve Settings pages no longer show "This page is not available yet" on any row: Appearance, System Settings, Notifications, Console, Data Views, Deployment options, Terminal, Commit, Confirmation, Git, Font and Code Style. Each own control stages its value, Apply stores the setting the webview's page writes, Cancel and Reset behave as for standard controls, and the search and the modified marker see them. The MCP Server page's Endpoint is left out: the native app has no MCP server.
- Recorded as done: Notifications and Deployment options (neither has a number field). Git was first recorded as done and corrected to partial the same day: it has a number field. The others stay partial: an out-of-range number is flagged without the webview's "Enter a value between…" hint; Appearance lacks Sync with OS, preferred themes, plugin icon themes and a colour picker; Font has no preview and does not apply the bold weight; Code Style has no General / Formatter tabs. The end-to-end test of a page operates two to four of its own controls; the remaining ones were compared with the webview's page by reading.
- The editor now takes its family, fallback, size, line height, ligatures and weight from the font settings (before, the font came from the theme and the settings had no effect). Diff views, log editors and the language popups still use the theme's font, where the webview gives them the editor font.
- Opening Settings no longer lists the installed fonts, looks for `git` or asks for the login shell on the window's thread; each is read off it when the page that needs it is shown. A test build reads no system font.
- Passwords › Forget is proven through the file store of the test home; the OS credential store is never set up in a test.
- A trusted project's own plugins load when the project opens and when Trust Project is chosen, and unload on Revoke Trust and when the project closes, without the Plugins page; their commands, menu items, status items, tool windows and settings page appear and disappear with them. The webview does not unload on close (they stop being listed); the native app unloads when no other window has the project open. A freshly loaded project plugin still waits for the consent given on the Plugins page; the tests grant it through the host.
- Not checked: with two windows, one window may show the plugin contributions of the other window's project (the listing is merged; the webview lists per window).
- Found, to fix next: the list of installed fonts is read from the operating system in thirteen places, some of them on paint paths.

## D94. Installed fonts are read once; the editor's remaining actions; Zen mode and fold chevrons

- Six views asked the operating system for the list of every installed font on each frame (the editor, the diff view, the merge editor, the terminal and the two usages views; only tests had ever filled the cache the editor looked at). One global now holds the list, read once off the window's thread when the application starts and again only from the Settings refresh button; until it arrives, text uses the theme's first monospace family and the windows repaint when it does. One function is the only caller of the system list; it panics in a test build, and a test draws the editor, a diff and a terminal for several frames and finds no read. This matters for the measurement: before it, the native app paid this cost on every paint.
- Still read on paint paths, not yet changed: the terminal's font and two status-bar and tool-window-bar checks copy the whole settings value under a lock each frame (hypothesis that it costs something measurable; a profile would say).
- Expand to Level and Expand All to Level (ten actions), Context Info, navigation to a custom region, Unwrap, Expression Type, the Reformat File dialog, Rerun, Next and Previous Occurrence, rainbow brackets and `project.refresh` are in the application, each with a key press in the editor and a key press where its condition is false.
- Not registered, on purpose: Toggle Semantic Highlighting (there is no semantic-token pipeline to switch) and Add or Remove Caret as an action (registering its empty handler made the window swallow the ⌥⇧-click that adds a caret; the click itself works).
- Reload All from Disk reaches `project.refresh`, but no test can tell the reload from the file watcher, which refreshes the tree by itself: the row stays unproven.
- Go to Test on a file without tests opens no chooser. The webview opens "Choose Test for X (0 found)" with only Create New Test…, a row that would do nothing natively because file templates are not built. A drafted sentence that the webview does not have ("No tests found") was removed.
- The gutter has a fold column, the rightmost one, as in the webview: chevrons fold and unfold on click without moving the caret, changing the selection, setting a breakpoint or running a plugin's gutter action, and presses there never set a breakpoint. Existing breakpoint and gutter-menu tests press further left now.
- Distraction Free mode hides the tab strip, the gutter and plugin gutter icons and centres the text at the configured width, as the webview does.
- The status bar shows the highlighted usages count and a transient "N of M" message while stepping through occurrences.
- Three IPC rows (`window_new`, `window_cycle`, `menu_refresh`) are recorded on equivalent native behaviour: their adapters call functions that exist only in the Tauri shell.

## D95. A project's own plugins stay in their window; the range hint; proofs of backend operations

- Measured before the fix, with two project windows: a trusted project's own plugins showed in the other window (listed, action enabled, Tools menu row, status item, tool window, settings page) and their command ran from it, whether the other window's project was trusted or restricted. The backend does not scope a plugin command to a window's project; only its listing does.
- Now each window lists the installed plugins plus the plugins of its own trusted project; a project plugin's command carries its project and is refused in any other window; Find Action leaves such actions out; a restricted window or one without a project shows installed plugins only. Closing a window, not only the last one, closes its backend project and releases its plugins (before, a closed window's project stayed open in the backend).
- Open, to settle against the Tauri side: an installed (not project) plugin's command still runs natively in a restricted window and in a single-file window. The webview refuses plugin commands for an untrusted window in its central guard; adding the same check natively broke the existing single-file plugin test, so it was left out pending a reading of the guard.
- A number field out of range shows "Enter a value between {min} and {max}" as its tooltip, on standard and own-control fields. With it, six Settings pages are recorded done (Console, Data Views, Terminal, Commit, Confirmation, Git), on tests of some of their own controls and a reading of the rest against the webview's page.
- 25 proof tests of backend operations are in the build (plugins marketplace from local files, package install, roll back, permissions, the debugger's capabilities, threads, breakpoint states, exception, function and data breakpoints, listening for a JVM on loopback, PHP and .NET resolution with stand-in scripts, database export, agent settings). A breakpoint the debugger reports unbound is drawn as a grey dot, as the webview draws it.
- Left out: the Git-init test of New Project (the backend's `git init` reads the machine's Git configuration and cannot be isolated without changing the process environment), the Attach test (the Project view cannot show a second root), and two database tests that need the console of the pending Database slice.
- The test binary had reached 8,052 threads of the 8,192 macOS allows per process: every test's backend kept ten worker threads until the binary exited, and two whole-suite runs hung. Test builds now give each backend two workers. Seen in the same sample and not yet explained: hundreds of file-watcher, database-history and index threads left behind; whether a closed project's threads stop in the application is to be measured before the comparison with the Tauri build.

## D96. Plugins in a window that is not trusted; quit; a rule for spec cases without rendering; the suite under load

- The Tauri app refuses, in one central place, every command that is not on a short allow-list for a window whose project is not trusted. That covers a project in Restricted Mode and a single-file window; a window with no project is refused too, except for plugin management. Of the plugin commands only subscribing, listing, the active file, themes, the catalog, assets, the marketplace and packages are allowed. Natively every plugin call already asked the backend the same question except the one behind actions and menus; it asks now, and the refusal shows as the "… failed" notification the webview shows. Nine tests, one per entry point, each with a refused case and an allowed one. The test that ran a plugin command in a single-file window was wrong by this rule and was replaced.
- Earlier statements corrected: plugin views, gutter providers, settings actions, prompts and forwarded events were already refused in a restricted window; only the action path ran.
- Quit closes each window that agreed before the application quits, as the Tauri shell does; before, the windows stayed. Restart goes the same way.
- A project's threads end when it leaves its window: measured in one process with one backend, none of the file-watcher, index or database-history threads is left after closing, after ten open and close rounds, or after ten windows. The hundreds seen in the test binary belong to tests that never close their projects. The test reads thread names through a test-only system call, the crate's only `unsafe`.
- A rule for spec rows, applied from here on: a case of a `.test.tsx` spec that asserts a pure function and renders nothing is proven by a Rust test of the equivalent function, provided the application reaches that function; cases that render or interact still need a test through a view or a window.
- The Reformat File dialog always offers Optimize imports, as the webview does for every file but a database console; an earlier note of mine that this depended on the language server was wrong.
- The suite is not stable under machine load: three of my five whole-suite runs passed (6,800 tests); two runs while other applications loaded the machine failed three tests waiting for something the file watcher should deliver. The cause is not known; it is the next integration's only first task.

## D97. Why the suite failed under load, and what it found in the application

- The failures of D96 were reproduced by stress (7 failures in 150 process-rounds; two whole suites at once failed 5 and 6 tests each) and classified from the dumps of the waits that ran out.
- The main cause was in the tests, with a real limit behind it: no test ever released its window and project (quitting is a no-op on the test platform), so one test process held over 400 FSEvents streams. A probe showed that the 513th live stream of a process never delivers an event, and that the limit is lower when several processes hold streams. Tests now close their projects when their thread ends (about 14 streams at most). The earlier hypothesis, an event lost right after the first status read, was wrong.
- In the backend (`crates/ide-vfs`), the watcher now counts its FSEvents streams and refuses one past 512, falling back to polling with the existing "sync may be slow" warning instead of watching nothing silently. The Tauri app shares this code and only gains the guard. The polling fallback misses an edit made in the same second as its last scan (measured), which is why it is the fallback and not the default. The lower shared limit across processes is not guarded.
- Three defects of the application were found and fixed, each with a test that fails without the fix: the Commit panel read the repository status on its own, outside the window's Git gate, so a Git command started right after a commit (a rebase after a fixup) could fail on `index.lock` (28 failures in 150 before, 0 after); a folder of the Project view asked to reload while it was being read stayed stale until the next change on disk; closing a tab that could not be saved saved it twice and raised two notices.
- Four tests asserted before the state they check existed and now wait for it; the short wait helper has the same 30-second budget as the long one. After the fixes: 0 failures in 150 process-rounds under a load average of 57 to 114, ten whole-suite runs green (two of them beside a second whole suite), and three of mine under load.
- Still open: Stop sends an interrupt signal first, and a process started with that signal ignored (for example the IDE started through `nohup`, or a test binary run as a background job of a script) does not stop; the code is in `crates/ide-run`, shared with the Tauri app. Not fixed; whether the Tauri app shows it was not checked.
- Two batches of proof tests landed with it (33 tests). Five spec files are complete by view tests (caret commands, the chunk optimizer through the optimizer the diff and the merge really use, the Markdown preview, the File Structure popup, the agent reply) and four more by the rule of D96 for cases that render nothing (build, file refactoring, the pinned chooser, removal of run configurations). Fixed on the way: a Cancel pressed before a file refactoring's search starts now stops it; starting with Debug marks the configuration recent; the Git log's User filter reads every repository root.
