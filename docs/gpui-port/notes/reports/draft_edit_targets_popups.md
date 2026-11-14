I've written the three pieces as six new files plus the notes file. None of it has been compiled. No existing file was edited: every change to a compiled file is written as an exact hunk in the notes, with its anchor checked to occur once.

**Files written**

- `app-gpui/src/platform/edit_target.rs` (240 lines): the `EditTarget` trait, a registry keyed by focus handle, and the text fields' undo history.
- `app-gpui/src/workbench/edit_actions.rs` (522): puts the six `edit.*` actions in front of the editor's handlers, implements the trait for `TextField`, `TextArea`, `SecretField`, `ProjectTree` and `ServicesView`, provides the Edit menu's greyed rule, and has 7 end-to-end tests.
- `app-gpui/src/ui/popup_anchor.rs` (96): where each popup goes (under the caret, under its button, or its fallback spot).
- `app-gpui/src/workbench/popup_placement.rs` (376): the popup slot can draw a popup at a point, Go to Line is a modal dialog, 7 end-to-end tests.
- `app-gpui/src/vcs/shelf_time.rs` (116): the shelf row's time, with a fixed-clock view test.
- `app-gpui/src/platform/error_log_sink.rs` (133): per-window failure lines written to the log under target `frontend`.
- Notes: `/private/tmp/claude-502/-Users-davidvieira--claude-personal-spares-spare-BDrKLZ/ef45d052-1f0c-4be6-a975-4f7e63cf4971/scratchpad/drafts/edit_targets_popups_integration.md` (1111 lines).

**A. Edit actions on text fields and lists**

- **Built:** Edit commands go to whatever has the keyboard. In the editor, behaviour is unchanged; it now routes through `route_edit_command`, so that model is reached.
- **Text fields:** they get undo and redo, with a run of typing undone in one step and ⌘Z / ⌘⇧Z as keys. A copy from a field goes into the clipboard history.
- **Password fields:** copy and cut do nothing; paste, select all and undo still work.
- **Project view:** Undo restores its last file operation; the other commands do nothing, so the editor behind it is no longer changed.
- **Services tree:** Select All and ⌘A select every row.
- **Tests:** the editor's own history including a programmatic edit; a dialog's field through the registry and the keys, with the greyed state; the Find in Files field; the clipboard history spec case; the Project view; the Services tree; the password field.
- **Rows finished:** `edit.copy`, `edit.cut`, `edit.paste`, `edit.redo`, `edit.selectAll`, both clipboard-history rows, `editCommand.test`. `edit.undo` stays partial (undo of a move, and ⌘Z in the tree).

**B. Go to Line as a dialog, popups anchored at the caret or button**

- **Built:** Show Usages opens at the caret line's bottom + 4 px, the mnemonic chooser at + 6, and Select In at + 2 when the file is the one being edited. Add Service opens 2 px under its button.
- **Go to Line:** a centred modal dialog. A click beside it does not close it, Escape gives the keyboard back to the editor, and Enter moves the caret and focuses the editor.
- **Tests:** go-to-line covered in 4 tests (`line:column` and comma syntax, the hint, the key doing nothing while a tool window has focus); Show Usages, the mnemonic chooser and Select In each have one bounds test (3 tests).
- **Add Service's test** needs helpers that are private to the workbench tests, so it is given as a hunk for `proofs3_run.rs`.
- **Rows finished:** `navigate.gotoLine`, `editor.gotoLine`, `find.showUsages`, `usagesPopup`, `toggleBookmarkWithMnemonic`. `selectIn`, `addService` and `Menu.tsx` stay partial for their other gaps.

**C. Shelf time and error log sink**

- **Shelf rows** now show "N files · <time>" in the Date Formats settings, and show nothing extra when the time is 0. This finishes `vcs.shelve` and `vcs.showShelf`.
- **Error log sink:** the sink is built and tested, but nothing reports to it yet, so both errorLog rows stay partial.

**Hunks in compiled files** (in the notes): `platform/actions.rs` (`Action::run_handler`), `platform/mod.rs`, `ui/mod.rs`, `vcs/mod.rs`, `workbench/mod.rs`, `ui/text_field.rs`, `ui/text_area.rs`, `ui/secret_field.rs`, `project_tree.rs`, `services/view.rs`, `workbench/services_window.rs`, `workbench/code_insight.rs`, `workbench/project_view_actions.rs`, `workbench/menu.rs`, `vcs/shelf_view.rs`. In `workbench/mod.rs` this includes updating the existing test `go_to_line_and_bookmarks_move_the_caret_through_the_project`, which still expects the old popup.

**Designed but not coded, and why**

- **`edit.delete`:** not in the action manifest, so it can't be registered.
- **macOS Edit menu greying:** those items are OS roles hard-coded as enabled; greying them means changing the menu enum, which other code matches on.
- **Copying list rows:** the webview defines no text for it, so lists and trees take Copy and do nothing.
- **What feeds the error sink:** the native app has no "uncaught error" event. A failing action handler is my guess at the counterpart; it needs your decision, and a one-line hunk is in the notes.
- **Error log file and `help.showLog`:** they need `tracing-appender` and `dirs`, which aren't dependencies.

**Differences from the assignment**

- **No range hint:** the webview's Go to Line has none. It shows "Current position L:C" or "No active editor", which the native dialog already draws, and the tests assert that.
- **Greying the Edit items on Windows/Linux:** the webview never does this. It's built as you asked, but you may prefer it limited to macOS.

**Check first when integrating**

1. `edit_actions::register_actions` must run after `editor::actions::register`. It does in `init` and in the test `start` helper; any setup that registers the editor's actions afterwards silently drops the wrappers.
2. The `register(...)` lines in the five view constructors only compile together with the `edit_actions` module and `run_handler`.
3. The bounds tests use exact pixel origins, which assumes `anchored().snap_to_window()` doesn't move the popup in the default test window.

Tests most likely to fail first:

- **The dialog-field key test:** on macOS ⌘⇧Z goes through the field's new redo branch.
- **The Project view test:** off macOS it also builds the whole menu state.
- **The Services tree test:** it runs in a window without a workbench.
