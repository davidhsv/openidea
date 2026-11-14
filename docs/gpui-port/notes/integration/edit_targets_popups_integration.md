# Integrating Edit targets, anchored popups, Go to Line as a dialog, shelf times and the error log sink

New files, not compiled yet:

| File                                        | Piece                                                                                                                                                                                  |
| ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app-gpui/src/platform/edit_target.rs`      | A: the `EditTarget` trait, the registry by focus handle, the text fields' undo history                                                                                                 |
| `app-gpui/src/workbench/edit_actions.rs`    | A: the Edit actions put in front of the editor's, the `EditTarget` impls (TextField, TextArea, SecretField, ProjectTree, ServicesView), the Edit menu's greyed state, end-to-end tests |
| `app-gpui/src/ui/popup_anchor.rs`           | B: where each popup goes (caret / button / fallback)                                                                                                                                   |
| `app-gpui/src/workbench/popup_placement.rs` | B: the popup slot drawing an anchored popup, Go to Line:Column as a dialog, end-to-end tests                                                                                           |
| `app-gpui/src/vcs/shelf_time.rs`            | C: the shelf row's time, with a fixed-clock view test                                                                                                                                  |
| `app-gpui/src/platform/error_log_sink.rs`   | C: per-window reporting of failures to the log (target `frontend`)                                                                                                                     |

Every anchor below was checked with `grep -cF` to occur once in the file as it was when written (2026-11-14). Line numbers are left out because `mod.rs` moves. Run `cargo fmt` after pasting: some pasted lines may exceed 120 columns.

All sections of a piece are required for that piece to compile; pieces A, B and C are independent of each other except that B's section B4 and A's section A9 both touch `workbench/mod.rs`.

---

## Piece A: Edit actions on text fields and lists

### A1. `mod` lines

`app-gpui/src/platform/mod.rs`, after `pub mod dispatcher;`:

```rust
pub mod edit_target;
```

`app-gpui/src/workbench/mod.rs`, after `mod editor_core;`:

```rust
mod edit_actions;
```

### A2. `app-gpui/src/platform/actions.rs`: run a wrapped handler

After the method

```rust
    pub fn is_toggled(&self, window: &Window, cx: &App) -> Option<bool> {
        self.toggled.as_ref().map(|toggled| toggled(window, cx))
    }
```

add (inside `impl Action`):

```rust
    /// Runs the handler alone, without the `when` check or the listeners: for a handler that puts
    /// itself in front of another one.
    pub fn run_handler(&self, call: &ActionCall, window: &mut Window, cx: &mut App) -> anyhow::Result<()> {
        (self.run)(call, window, cx)
    }
```

### A3. `app-gpui/src/workbench/mod.rs`: register the wrappers

In `pub fn register_actions(cx: &mut App)`, after `shelf::register_actions(registry);` add:

```rust
    edit_actions::register_actions(registry);
```

This must run after `crate::editor::actions::register(cx)` (it wraps the handlers registered there). It does in `lib.rs::init` and in `workbench::tests::start`. Any other test setup that calls `editor::actions::register` after `workbench::register_actions` loses the wrappers silently (behaviour falls back to the old editor-only handling).

### A4. `app-gpui/src/workbench/menu.rs`: Edit items greyed out by the target

Replace

```rust
        .filter(|id| registry.get(id).is_some_and(|action| !is_action_enabled(&action.meta, &context)))
```

with

```rust
        .filter(|id| {
            registry.get(id).is_some_and(|action| !is_action_enabled(&action.meta, &context))
                || super::edit_actions::edit_item_disabled(id, window, cx)
        })
```

This reaches the Windows/Linux Edit items (`hinted("edit.copy", …)` in `menu_spec.rs`). On macOS the Edit items are `Entry::Native` roles built with `disabled: false` (see "not built").

### A5. `app-gpui/src/ui/text_field.rs`

a) Import. After `use std::ops::Range;` add:

```rust
use crate::platform::edit_target::{TextHistory, TextState};
```

b) The field. Replace

```rust
    /// Typing, pasting and deleting change nothing; the text can still be selected and copied.
    read_only: bool,
}
```

with

```rust
    /// Typing, pasting and deleting change nothing; the text can still be selected and copied.
    read_only: bool,
    /// The steps Undo and Redo walk.
    history: TextHistory,
}
```

c) Registration and initial value. Replace

```rust
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
```

with

```rust
    pub fn new(cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        crate::platform::edit_target::register(&focus, cx.weak_entity(), cx);
        Self {
            focus,
```

and replace `            read_only: false,` with

```rust
            read_only: false,
            history: TextHistory::default(),
```

d) `set_text` forgets the history. Replace

```rust
        (self.anchor, self.head) = (self.text.len(), self.text.len());
```

with

```rust
        (self.anchor, self.head) = (self.text.len(), self.text.len());
        self.history.clear();
```

e) Methods. Before `    pub fn is_read_only(&self) -> bool {` add:

```rust
    /// The text and selection now, for the undo history.
    fn snapshot(&self) -> TextState {
        TextState { text: self.text.clone(), anchor: self.anchor, head: self.head }
    }

    /// Puts back a state of the history; the owner hears of the change.
    fn restore(&mut self, state: TextState, cx: &mut Context<Self>) {
        self.text = state.text;
        (self.anchor, self.head) = (state.anchor.min(self.text.len()), state.head.min(self.text.len()));
        self.marked = None;
        cx.emit(TextFieldEvent::Changed);
        cx.notify();
    }

    pub fn can_undo(&self) -> bool {
        !self.read_only && self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        !self.read_only && self.history.can_redo()
    }

    /// Takes back the last edit (a run of typing is one); false when there is none.
    pub fn undo(&mut self, cx: &mut Context<Self>) -> bool {
        if self.read_only {
            return false;
        }
        let Some(previous) = self.history.undo(self.snapshot()) else { return false };
        self.restore(previous, cx);
        true
    }

    /// Makes again what [`Self::undo`] took back; false when there is nothing.
    pub fn redo(&mut self, cx: &mut Context<Self>) -> bool {
        if self.read_only {
            return false;
        }
        let Some(next) = self.history.redo(self.snapshot()) else { return false };
        self.restore(next, cx);
        true
    }

    /// Puts the selected text on the clipboard and in the clipboard history of Paste from History;
    /// false with nothing selected.
    pub fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self.selected_text().to_owned();
        if selected.is_empty() {
            return false;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(selected.clone()));
        cx.default_global::<crate::editor::view::EditorClipboard>().0.record(&selected, false);
        true
    }

    /// Copies the selection and deletes it (a read-only field keeps it); false with nothing selected.
    pub fn cut(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.copy(cx) {
            return false;
        }
        self.replace(None, "", cx);
        true
    }

    /// Replaces the selection with the clipboard's text; false when the clipboard holds none.
    pub fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        match cx.read_from_clipboard().and_then(|item| item.text()) {
            Some(text) => {
                self.replace(None, &text, cx);
                true
            }
            None => false,
        }
    }
```

f) Recording in `replace`. Replace

```rust
        let range = range.unwrap_or_else(|| self.selection());
```

with

```rust
        let range = range.unwrap_or_else(|| self.selection());
        // One character typed over no selection joins the run of typing before it.
        let typing = range.is_empty() && new_text.chars().count() == 1;
        self.history.record(self.snapshot(), typing);
```

g) A caret move ends the run of typing. Replace

```rust
    fn move_to(&mut self, to: usize, extend: bool, cx: &mut Context<Self>) {
```

with

```rust
    fn move_to(&mut self, to: usize, extend: bool, cx: &mut Context<Self>) {
        self.history.break_run();
```

h) The keys. Replace the block

```rust
        if modifiers.secondary(cx) && !modifiers.alt && !modifiers.shift {
            let handled = match key {
                "a" => {
                    self.select_all(cx);
                    true
                }
                "c" | "x" if !selection.is_empty() => {
                    cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text().to_owned()));
                    if key == "x" {
                        self.replace(None, "", cx);
                    }
                    true
                }
                "v" => match cx.read_from_clipboard().and_then(|item| item.text()) {
                    Some(text) => {
                        self.replace(None, &text, cx);
                        true
                    }
                    None => false,
                },
                _ => false,
            };
```

with

```rust
        if modifiers.secondary(cx) && !modifiers.alt && modifiers.shift && key == "z" && self.redo(cx) {
            cx.stop_propagation();
            return;
        }
        if modifiers.secondary(cx) && !modifiers.alt && !modifiers.shift {
            let handled = match key {
                "a" => {
                    self.select_all(cx);
                    true
                }
                "c" => self.copy(cx),
                "x" => self.cut(cx),
                "v" => self.paste(cx),
                "z" => self.undo(cx),
                _ => false,
            };
```

(`selection` stays in use further down; if clippy reports it unused in that block, nothing else changes.)

### A6. `app-gpui/src/ui/text_area.rs` (the commit message and other multi-line fields)

a) Registration. Replace

```rust
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
```

(in `impl TextArea`; `            focus: cx.focus_handle(),` occurs once in the file) with

```rust
    pub fn new(cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        crate::platform::edit_target::register(&focus, cx.weak_entity(), cx);
        Self {
            focus,
```

b) Methods. Before `    fn editable(&self) -> bool {` add:

```rust
    pub fn can_undo(&self) -> bool {
        self.editable() && !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        self.editable() && !self.redo.is_empty()
    }

    /// Puts the selected text on the clipboard and in the clipboard history of Paste from History;
    /// false with nothing selected.
    pub fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let selected = self.selected_text().to_owned();
        if selected.is_empty() {
            return false;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(selected.clone()));
        cx.default_global::<crate::editor::view::EditorClipboard>().0.record(&selected, false);
        true
    }

    /// Copies the selection and deletes it unless the area is read-only; false with nothing selected.
    pub fn cut(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.copy(cx) {
            return false;
        }
        if !self.read_only {
            self.edit(None, "", EditKind::Other, cx);
        }
        true
    }

    /// Replaces the selection with the clipboard's text; false when it holds none or the area is not
    /// editable.
    pub fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.editable() {
            return false;
        }
        match cx.read_from_clipboard().and_then(|item| item.text()) {
            Some(text) => {
                self.edit(None, &text, EditKind::Other, cx);
                true
            }
            None => false,
        }
    }
```

c) Keys. Replace

```rust
                ("c" | "x", false) if !selection.is_empty() => {
                    cx.write_to_clipboard(ClipboardItem::new_string(self.selected_text().to_owned()));
                    if key == "x" {
                        self.edit(None, "", EditKind::Other, cx);
                    }
                    true
                }
                ("v", false) if editable => match cx.read_from_clipboard().and_then(|item| item.text()) {
                    Some(text) => {
                        self.edit(None, &text, EditKind::Other, cx);
                        true
                    }
                    None => false,
                },
```

with

```rust
                ("c", false) => self.copy(cx),
                ("x", false) => self.cut(cx),
                ("v", false) if editable => self.paste(cx),
```

Assumption: the original `x` path called `self.edit` even when read-only and relied on `edit` refusing; `cut` now checks `read_only` itself. Check `text_area.rs`'s tests still pass (`selection` may become unused there: remove the binding if clippy says so).

### A7. `app-gpui/src/ui/secret_field.rs`

a) Registration. Replace

```rust
        Self { focus: cx.focus_handle(), state, placeholder: SharedString::default(), _state_events: events }
```

with

```rust
        let focus = cx.focus_handle();
        crate::platform::edit_target::register(&focus, cx.weak_entity(), cx);
        Self { focus, state, placeholder: SharedString::default(), _state_events: events }
```

b) The edited state, for the Edit commands. Before `    /// Empties the field.` add:

```rust
    /// The edited text and selection, for the Edit commands; never drawn.
    pub(crate) fn state(&self) -> &Entity<TextField> {
        &self.state
    }
```

c) ⌘Z / Ctrl+Z undoes. Replace

```rust
                "c" | "x" => {}
```

with

```rust
                "c" | "x" => {}
                "z" => {
                    self.state.update(cx, |state, cx| state.undo(cx));
                }
```

### A8. `app-gpui/src/project_tree.rs`

Replace

```rust
        let mut tree = Self {
            focus: cx.focus_handle(),
```

with

```rust
        let focus = cx.focus_handle();
        crate::platform::edit_target::register(&focus, cx.weak_entity(), cx);
        let mut tree = Self {
            focus,
```

### A9. `app-gpui/src/services/view.rs` (registration and ⌘A; B6 adds the Add button's bounds in the same file)

a) Replace

```rust
    pub fn new(source: Rc<dyn ServicesSource>, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
```

with

```rust
    pub fn new(source: Rc<dyn ServicesSource>, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        crate::platform::edit_target::register(&focus, cx.weak_entity(), cx);
        Self {
            focus,
```

b) Replace

```rust
    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
```

with

```rust
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        // ⌘A / Ctrl+A selects every row, as Edit › Select All does here.
        if event.keystroke.key == "a" && modifiers.secondary(cx) && !modifiers.alt && !modifiers.shift {
            let command = crate::app_models::edit_command::EditCommand::SelectAll;
            <Self as crate::platform::edit_target::EditTarget>::edit(self, command, window, cx);
            cx.stop_propagation();
            return;
        }
```

### Assumptions (A)

- Assumption: `cx.weak_entity()` is callable inside `TextField::new` / `ProjectTree::new` / `ServicesView::new` / `SecretField::new` / `TextArea::new` (it is in `GotoLine::new`).
- Assumption: `FocusHandle::contains(&self, other, window)` reflects the last frame; a target registered around the focused one but drawn for the first time this frame is not seen as containing it yet. Exact focus matches (all the tests) do not depend on it.
- Assumption: the macOS menu shortcut bindings (`native.copy` → Meta+C …) do not stop the keystroke before the focused view's `on_key_down` (menu.rs `run` propagates a key in flight). The text-field key tests rely on it; they already pass for ⌘C/⌘X/⌘V today.
- Assumption: `ServicesView::click` on the first row and Shift-click on the last selects every row; the lead ends on the last row (the webview keeps the lead where it was).

### Compile risks (A)

- `register<T: EditTarget>` is called in `ui/` and `project_tree.rs` for types whose `EditTarget` impl is in `workbench/edit_actions.rs`: legal (same crate), but the impl only exists once `mod edit_actions;` is added; A1, A3 and A5–A9 go in together.
- `run_handler` (A2) must be added before `edit_actions.rs` compiles.
- In A4, the closure uses `cx` (`&mut App`) while `registry` borrows it shared: if the borrow checker refuses, bind `let app: &App = cx;` before `let registry` and use `app` for both.
- `entity.update(cx, |target, cx| … target.edit(command, window, cx))` in `edit_target.rs` captures `window: &mut Window` in an `FnOnce`: fine for `Entity::update`.
- `is_none_or` on `Option` needs Rust ≥ 1.82 (already used in `menu_spec.rs`).

### Tests most likely to fail first (A)

- `the_edit_commands_act_on_the_text_field_of_a_dialog`: on macOS ⌘⇧Z is spelled `cmd-shift-z` by `primary("shift-z")`; if the dialog claims keys (Enter only today) or validation rewrites the text on `Changed` (it turns spaces into `-`), the asserted texts may differ; also `vcs.newBranch` needs the repository read (the test waits for the dialog).
- `in_the_project_view_the_edit_commands_leave_the_editor_alone`: on Windows/Linux `greyed` also asserts `menu::state(...).disabled`; `menu::state` may need globals the test window lacks (it builds the whole bar).
- `select_all_in_the_services_tree_selects_every_row`: runs in a plain window (no workbench); `execute_action_or_throw` there depends on `window_context` working without a workbench.
- `use_the_focused_editors_history_including_programmatic_edits`: `apply_edit` goes through the backend document; the waits cover it, but `edit.undo` may first try `undo_refactoring`.

### What the webview does that is not built (A)

- `edit.delete`: not in the manifest (`app-gpui/assets/gen/actions.json` has only the six ids, from `src/features/app/appActions.ts:99-104`); `registry.handle` panics on an unknown id.
- The macOS Edit menu items are OS roles (`Entry::Native` with `disabled: false` in `menu.rs`); WebKit greys them by the responder chain. Greying them natively needs `MenuNode::Native` to carry an enabled flag set from `edit_actions::edit_item_disabled(role_action(role))`. Designed, not coded: it changes the `MenuNode` enum that other code matches on.
- On Windows/Linux the webview never greys the hinted Edit items (they have no `when`; `disabledMenuActions` only checks `isActionEnabled`). The greyed state built here follows the assignment and the macOS behaviour; an owner may want it limited to macOS.
- Copy in a list or tree: the webview defines no row text for `edit.copy` (`execCommand("copy")` on a tree copies nothing; `Tree.tsx` has no copy handler; `project.copy` is a separate action). The tree and list targets therefore take Copy, Cut, Paste and Redo and do nothing, so that the editor behind them is not touched. Nothing is invented for row text.
- Select All in a tree: the webview's multi-select trees select every row on ⌘A (`src/ui/Tree.tsx:567-570`). Built for the Services tree only; the native Project view and the Shelf tree are single-selection, so their Select All does nothing.
- The Project view's Redo: the webview's undo handler takes only `undo` (`src/features/project-tree/index.ts:209-215`); Redo does nothing there, as built.
- Other text inputs (`ui/secret_field.rs` done; `editor/find_bar.rs`, `everywhere` search fields and the settings fields are `TextField`s and are covered by A5).

### `parity.json` (A)

| Key                                                   | Test                                                                                                                                                                                                                                             | Status after integration                                                                   |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------ |
| `action:edit.copy`                                    | `workbench::edit_actions::tests::the_edit_commands_act_on_the_text_field_of_a_dialog`, `…::records_the_selected_text_of_inputs`, `…::in_the_project_view_the_edit_commands_leave_the_editor_alone`, `…::a_password_field_is_never_copied_or_cut` | done                                                                                       |
| `action:edit.cut`                                     | same as copy, `…::the_edit_commands_act_on_the_find_in_files_field`                                                                                                                                                                              | done                                                                                       |
| `action:edit.paste`                                   | `…::the_edit_commands_act_on_the_text_field_of_a_dialog`, `…::the_edit_commands_act_on_the_find_in_files_field`, `…::a_password_field_is_never_copied_or_cut`                                                                                    | done                                                                                       |
| `action:edit.redo`                                    | `…::the_edit_commands_act_on_the_text_field_of_a_dialog`, `…::the_edit_commands_act_on_the_find_in_files_field`, `…::use_the_focused_editors_history_including_programmatic_edits`                                                               | done                                                                                       |
| `action:edit.selectAll`                               | `…::the_edit_commands_act_on_the_text_field_of_a_dialog`, `…::select_all_in_the_services_tree_selects_every_row`                                                                                                                                 | done                                                                                       |
| `action:edit.undo`                                    | `…::the_edit_commands_act_on_the_text_field_of_a_dialog`, `…::a_password_field_is_never_copied_or_cut`, `…::in_the_project_view_the_edit_commands_leave_the_editor_alone`                                                                        | partial: the Project view's Undo of a move and ⌘Z in the tree are still missing (gap text) |
| `unit:src/editor/core/clipboardHistory.test.ts`       | `editor_commands::clipboard_history::tests` + `workbench::edit_actions::tests::records_the_selected_text_of_inputs`                                                                                                                              | done (8 of 8)                                                                              |
| `module:src/editor/core/clipboardHistory.ts`          | as above, plus the existing editor proofs                                                                                                                                                                                                        | done                                                                                       |
| `unit:src/features/app/__tests__/editCommand.test.ts` | `app_models::edit_command::tests` + `workbench::edit_actions::tests::use_the_focused_editors_history_including_programmatic_edits`                                                                                                               | done (2 of 2; `route_edit_command` is now reached from `edit_actions::run`)                |

---

## Piece B: Go to Line as a dialog, popups anchored at the caret or their button

### B1. `mod` lines

`app-gpui/src/ui/mod.rs`, after `pub mod pattern_index;`:

```rust
pub mod popup_anchor;
```

`app-gpui/src/workbench/mod.rs`, after `mod plugins_window;`:

```rust
mod popup_placement;
```

### B2. `app-gpui/src/workbench/mod.rs`: the popup's anchor

a) Replace

```rust
    /// What had the keyboard when the popup opened.
    restore: Option<FocusHandle>,
    _events: Subscription,
}
```

(in `struct Popup`; `    restore: Option<FocusHandle>,` occurs once) with

```rust
    /// What had the keyboard when the popup opened.
    restore: Option<FocusHandle>,
    /// Where its top left corner goes; `None`: centred near the top of the window.
    at: Option<gpui_kit::Point<gpui_kit::Pixels>>,
    _events: Subscription,
}
```

b) Replace

```rust
        self.popup = Some(Popup { view: view.clone().into(), restore, _events: events });
```

with

```rust
        self.popup = Some(Popup { view: view.clone().into(), restore, at: None, _events: events });
```

c) In `fn render`, replace

```rust
                        .child(
                            div()
                                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(popup.view.clone()),
                        ),
```

with

```rust
                        .child(Self::popup_body(popup)),
```

### B3. `app-gpui/src/workbench/mod.rs`: Go to Line:Column as a dialog

a) The field. After

```rust
    /// The dialog a Git operation waits on.
    vcs_dialog: Option<branches::VcsDialog>,
```

add

```rust
    /// Go to Line:Column while it is open.
    goto_line: Option<popup_placement::GotoLineDialog>,
```

b) Its initial value: after `            vcs_dialog: None,` add

```rust
            goto_line: None,
```

c) The layer: replace

```rust
            .children(self.plugin_prompt_layer())
```

with

```rust
            .children(self.plugin_prompt_layer())
            .children(self.goto_line_layer())
```

d) `show_goto_line` opens the dialog. Replace the body of

```rust
    /// Opens Go to Line:Column for the active editor; the chosen place gets the caret.
    pub fn show_goto_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
```

i.e. everything from `        let position = self.editor().map(|editor| editor.read(cx).caret_position());` through the closing `        self.open_popup(&popup, events, window, cx);\n    }` of that function, with

```rust
        self.open_goto_line_dialog(window, cx);
    }
```

After this, both names of `use crate::editor::goto_line::{GotoLine, GotoLineEvent};` are unused in `mod.rs` (checked: their only other use was this function): delete that line. `popup_placement.rs` imports both itself.

e) The existing test `go_to_line_and_bookmarks_move_the_caret_through_the_project` asserts the old popup. In it replace

```rust
        run(cx, "navigate.gotoLine", None);
        assert!(workbench.read_with(cx, |w, _| w.popup.is_some()));
        cx.simulate_input("3:5");
        cx.simulate_keystrokes("enter");
        assert_eq!(at(cx), Some((alpha.clone(), (2, 4))));
        assert!(workbench.read_with(cx, |w, _| w.popup.is_none()));
```

with

```rust
        run(cx, "navigate.gotoLine", None);
        assert!(workbench.read_with(cx, |w, _| w.goto_line_dialog().is_some()));
        cx.simulate_input("3:5");
        cx.simulate_keystrokes("enter");
        assert_eq!(at(cx), Some((alpha.clone(), (2, 4))));
        assert!(workbench.read_with(cx, |w, _| w.goto_line_dialog().is_none()));
```

Also grep the tests for other `GotoLine` downcasts of `w.popup` (`grep -rn "GotoLine" app-gpui/src/workbench app-gpui/src/editor`), e.g. a status-bar caret click test in `chrome.rs` tests; switch them to `goto_line_dialog()`.

f) The context keys: while the dialog is open `popupOpen` is not set (it is a dialog). If a `modalOpen`/`dialogOpen` key exists for `vcs_dialog`, set it in `publish_context` for `self.goto_line.is_some()` too. Assumption: none is needed for the tests.

### B4. `app-gpui/src/workbench/mod.rs`: the mnemonic chooser at the caret

In `pub fn show_mnemonic_chooser`, replace

```rust
        self.open_popup(&chooser, events, window, cx);
```

with

```rust
        let at = self.editor_popup_at(crate::ui::popup_anchor::mnemonic_chooser, window, cx);
        self.open_popup_at(&chooser, events, at, window, cx);
```

### B5. `app-gpui/src/workbench/code_insight.rs`: Show Usages at the caret

Replace

```rust
        let events = cx.subscribe_in(&view, window, Self::usages_popup_event);
        self.open_popup(&view, events, window, cx);
```

with

```rust
        let events = cx.subscribe_in(&view, window, Self::usages_popup_event);
        let at = self.editor_popup_at(crate::ui::popup_anchor::usages_popup, window, cx);
        self.open_popup_at(&view, events, at, window, cx);
```

### B6. `app-gpui/src/workbench/project_view_actions.rs`: Select In at the caret

a) Replace

```rust
                items.extend(menu.items);
                self.show_project_chooser(&items, window, cx);
```

with

```rust
                items.extend(menu.items);
                let position = self.select_in_popup_at(&path, window, cx);
                self.show_project_chooser_at(&items, position, window, cx);
```

b) Replace

```rust
    fn show_project_chooser(&mut self, items: &[Item], window: &mut Window, cx: &mut Context<Self>) {
        let position = Self::chooser_position(window);
```

with

```rust
    fn show_project_chooser(&mut self, items: &[Item], window: &mut Window, cx: &mut Context<Self>) {
        let position = Self::chooser_position(window);
        self.show_project_chooser_at(items, position, window, cx);
    }

    /// [`Self::show_project_chooser`] with its corner at `position`.
    fn show_project_chooser_at(
        &mut self,
        items: &[Item],
        position: gpui_kit::Point<gpui_kit::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
```

(the rest of the old body, `let run = …; self.show_menu(items, position, run, window, cx);`, stays as the body of `show_project_chooser_at`).

### B7. `app-gpui/src/services/view.rs`: the Add Service button's bounds

a) Field: replace `    scroll: UniformListScrollHandle,` (in `pub struct ServicesView`) with

```rust
    scroll: UniformListScrollHandle,
    /// Where the toolbar's Add Service button was last drawn, for the menu it opens.
    add_button: Rc<std::cell::Cell<Option<gpui_kit::Bounds<Pixels>>>>,
```

and `            scroll: UniformListScrollHandle::new(),` with

```rust
            scroll: UniformListScrollHandle::new(),
            add_button: Rc::default(),
```

b) Accessor: before `    pub fn tree(&self) -> &ServicesTree {` add

```rust
    /// Where the toolbar's Add Service button was last drawn.
    pub fn add_button_bounds(&self) -> Option<gpui_kit::Bounds<Pixels>> {
        self.add_button.get()
    }
```

c) The button measures itself and opens its menu under itself. Replace

```rust
            .child(
                self.tool_button("services-add".to_string(), "plus", true, cx, menu(ServicesMenu::AddService))
                    .with_tooltip_shortcut("Add Service", "services.addService"),
            )
```

with

```rust
            .child({
                let measured = Rc::clone(&self.add_button);
                let measure =
                    gpui_kit::canvas(move |bounds, _, _| measured.set(Some(bounds)), |_, _, _, _| {}).absolute().size_full();
                let open = |view: &mut Self, _: &ClickEvent, cx: &mut Context<Self>| {
                    let position = crate::ui::popup_anchor::button_menu(view.add_button.get());
                    cx.emit(ServicesViewEvent::ShowMenu { menu: ServicesMenu::AddService, position });
                };
                self.tool_button("services-add".to_string(), "plus", true, cx, open)
                    .relative()
                    .child(measure)
                    .with_tooltip_shortcut("Add Service", "services.addService")
            })
```

If `menu` becomes unused in `render_toolbar`, it is still used by the View Options button; check.

### B8. `app-gpui/src/workbench/services_window.rs`: `services.addService` under the button

In the `registry.handle("services.addService", …)` handler, replace

```rust
            let width = f32::from(window.viewport_size().width);
            let position = gpui_kit::point(px((width / 2.0 - 120.0).max(0.0)), px(44.0));
            workbench.show_services_menu(ServicesMenu::AddService, position, window, cx);
```

with

```rust
            let view = workbench.services_window(window, cx).map(|services| services.view.clone());
            let button = view.and_then(|view| view.read(cx).add_button_bounds());
            let position = crate::ui::popup_anchor::button_menu(button);
            workbench.show_services_menu(ServicesMenu::AddService, position, window, cx);
```

(The same two `width`/`position` lines occur twice in the file: replace the occurrence directly under `registry.handle("services.addService", |_, window, cx| {`.) Assumption: `services_window(window, cx)` returns `Option<&…>` with a `view: Entity<ServicesView>` field, as `show_services_menu` uses it.

### B9. Test to add for Add Service: `app-gpui/src/workbench/tests/proofs3_run.rs`

It needs `run_project` and `program`, private to `workbench::tests`, so it goes beside `add_service_opens_its_menu_and_a_chosen_type_shows_its_configurations`. After that test add:

```rust
/// Add Service opens its menu under the Services toolbar's Add button, from the action and from the
/// button.
#[gpui_kit::test]
async fn add_service_opens_its_menu_under_its_button(cx: &mut TestAppContext) {
    use gpui_kit::px;
    let (workbench, cx, _folders, _root, _store) = run_project(cx, vec![program("nap", "/bin/sleep", "30")]);
    cx.update(|window, _| window.activate_window());
    run_action(cx, "services.show", None);
    cx.run_until_parked();
    let button = cx.debug_bounds("services-add").expect("the Add button is drawn");
    let under_button = |cx: &mut VisualTestContext| {
        cx.run_until_parked();
        let row = cx.debug_bounds("menu-row-0-0").expect("the menu's first row is drawn");
        let (dx, dy) = (row.left() - button.left(), row.top() - (button.bottom() + px(2.0)));
        assert!(dx >= px(0.0) && dx <= px(8.0), "{dx:?}");
        assert!(dy >= px(0.0) && dy <= px(8.0), "{dy:?}");
    };
    run_action(cx, "services.addService", None);
    assert!(workbench.read_with(cx, |w, _| w.open_menu::<Vec<NodeCommand>>()).is_some());
    under_button(cx);
    cx.simulate_keystrokes("escape");
    click_on(cx, "services-add");
    assert!(workbench.read_with(cx, |w, _| w.open_menu::<Vec<NodeCommand>>()).is_some());
    under_button(cx);
    close_backend_project(cx);
}
```

### Assumptions (B)

- Assumption: `gpui_kit::anchored().position(at).snap_to_window()` places the popup with its top left corner at `at` in window coordinates when nested in the popup slot's flex backdrop (it is how `PopupMenu` places itself today).
- Assumption: `Editor::position_of` is in window coordinates (it adds `layout.bounds.left()/top()`), as `EditorIntelligence::popup_position` uses it for popups.
- Assumption: the webview's `Popup` (`src/ui/Popup.tsx`, `offset = 4`, `placement = "bottom-start"`) applies to the Usages popup and the mnemonic chooser; the context menus (`showContextMenu`, `src/ui/Menu.tsx:428`) open at the exact point. So: Usages = caret bottom + 4; mnemonic = caret bottom + 2 + 4; Select In = caret bottom + 2; Add Service = button bottom + 2.
- Assumption: `services.addService` run while the Services tool window is hidden opens the menu before the button is drawn: it falls back to (100, 102) like the webview's `r?.left ?? 100`. The webview waits one animation frame (`requestAnimationFrame`) first; GPUI's `on_next_frame` does not run in tests, so it is not used.

### Compile risks (B)

- `popup_placement.rs` reads and writes `Workbench`'s private fields `popup` and `goto_line` and the private `Popup` struct from a child module: legal. `pub(super) fn popup_body(popup: &Popup)` exposes a private type at the same visibility (fine).
- `EditorPlace` is a `fn` pointer type with elided lifetimes; `crate::ui::popup_anchor::usages_popup` etc. coerce to it. If not, pass closures: `|editor, size, cx| popup_anchor::usages_popup(editor, size, cx)`.
- In `select_in_popup_at`, `editor.path().to_string_lossy() == path` compares `Cow<str>` with `&str`: if refused, write `== *path` or `.as_ref() == path`.
- `body.debug_selector(...)` on a plain `div()` inside `anchored()`.

### Tests most likely to fail first (B)

- `show_usages_opens_under_the_caret`: the usages popup opens only once the search has a first result (`carry_out_popup`); `wait_long` waits for it, but `snap_to_window` may move it if the window is small (the test window's default size); then the exact `assert_eq!` on the origin fails.
- `select_in_opens_its_chooser_under_the_caret_of_the_edited_file`: the first row's inset inside the menu frame is assumed ≤ 8 px.
- `go_to_line_is_a_dialog_and_enter_moves_the_caret_to_the_typed_line_and_column`: the click beside the dialog lands on the occluding layer; if it moves focus off the field, the test refocuses it before typing.

### What the webview does that is not built (B)

- The "range hint" named in the assignment does not exist in the webview: `src/editor/core/view/GotoLineDialog.tsx` shows `tx\`Current position ${line}:${col}\``or`tx("No active editor")` under the field, which the native view already draws (`GotoLine::hint`); the tests assert it.
- The dialog's title text is drawn by `GotoLine::render` (`i18n.tx("Go to Line:Column")`, matching `title="Go to Line:Column"` in `GotoLineDialog.tsx`); no accessor exists to assert it, and none is added (it would need a hunk in `editor/goto_line.rs`).
- Dragging the dialog by its title (the webview's `Dialog` drags) is not built.
- Show Usages re-anchoring on scroll (floating-ui `autoUpdate`) is not built: the point is taken once when the popup opens.
- Select In's contributed targets and Navigation Bar row (other gaps of `navigate.selectIn`).

### `parity.json` (B)

| Key                                              | Test                                                                                                                                                                                                                                                                                                                                                       | Status after integration                                                              |
| ------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `action:navigate.gotoLine`                       | `workbench::popup_placement::tests::go_to_line_is_a_dialog_and_enter_moves_the_caret_to_the_typed_line_and_column`, `…::go_to_line_takes_a_line_a_line_and_column_and_a_comma`, `…::escape_closes_go_to_line_and_the_editor_has_the_keyboard_again`, `…::the_go_to_line_key_runs_nothing_while_a_tool_window_has_the_keyboard`, `editor::goto_line::tests` | done                                                                                  |
| `point.registerOverlay:editor.gotoLine`          | same                                                                                                                                                                                                                                                                                                                                                       | done                                                                                  |
| `action:find.showUsages`                         | `workbench::popup_placement::tests::show_usages_opens_under_the_caret` + existing                                                                                                                                                                                                                                                                          | done                                                                                  |
| `point.registerOverlay:code-insight.usagesPopup` | same                                                                                                                                                                                                                                                                                                                                                       | done                                                                                  |
| `action:navigate.toggleBookmarkWithMnemonic`     | `workbench::popup_placement::tests::the_mnemonic_chooser_opens_under_the_caret` + existing                                                                                                                                                                                                                                                                 | done                                                                                  |
| `action:navigate.selectIn`                       | `workbench::popup_placement::tests::select_in_opens_its_chooser_under_the_caret_of_the_edited_file`                                                                                                                                                                                                                                                        | partial: contributed targets and the Navigation Bar row remain                        |
| `action:services.addService`                     | `workbench::tests::proofs3_run::add_service_opens_its_menu_under_its_button` (B9)                                                                                                                                                                                                                                                                          | partial: a type with several configurations still adds all of them without the picker |
| `module:src/ui/Menu.tsx`                         | B9 test (menu anchored to a button)                                                                                                                                                                                                                                                                                                                        | partial: header rows, check marks and menu aim remain                                 |

---

## Piece C: shelf rows show their time; error log sink

### C1. `mod` lines

`app-gpui/src/vcs/mod.rs`, before `pub mod shelf_view;`:

```rust
pub mod shelf_time;
```

`app-gpui/src/platform/mod.rs`, after `pub mod edit_target;` (A1) or, without piece A, after `pub mod dispatcher;`:

```rust
pub mod error_log_sink;
```

### C2. `app-gpui/src/vcs/shelf_view.rs`

a) Import: after `use crate::vcs_diff::model::file_count;` add

```rust
use super::shelf_time::{ShelfClock, date_options, shelf_detail};
```

b) Field: replace

```rust
    selected: Option<String>,
}
```

(in `pub struct ShelfPanel`; `    selected: Option<String>,` occurs once) with

```rust
    selected: Option<String>,
    /// What the shelves' times are dated against.
    clock: ShelfClock,
}
```

c) Constructor: replace

```rust
        Self { focus: cx.focus_handle(), shelves: Vec::new(), loaded: false, expanded: HashSet::new(), selected: None }
    }
```

with

```rust
        Self {
            focus: cx.focus_handle(),
            shelves: Vec::new(),
            loaded: false,
            expanded: HashSet::new(),
            selected: None,
            clock: ShelfClock::default(),
        }
    }

    /// Dates the shelves against `clock` instead of the wall clock.
    pub fn set_clock(&mut self, clock: ShelfClock, cx: &mut Context<Self>) {
        self.clock = clock;
        cx.notify();
    }
```

d) `lines`: replace

```rust
    pub fn lines(&self, cx: &mut App) -> Vec<String> {
        let i18n = &mut cx.default_global::<Localization>().0;
```

with

```rust
    pub fn lines(&self, cx: &mut App) -> Vec<String> {
        let options = date_options(cx);
        let i18n = &mut cx.default_global::<Localization>().0;
```

and replace

```rust
                    None => {
                        let count = self.shelves.iter().find(|entry| entry.id == row.key).map_or(0, |e| e.files.len());
                        file_count(count, i18n)
                    }
```

with

```rust
                    None => match self.shelves.iter().find(|entry| entry.id == row.key) {
                        Some(entry) => shelf_detail(entry.files.len(), entry.time_ms, self.clock, &options, i18n),
                        None => file_count(0, i18n),
                    },
```

e) `render_rows`: replace

```rust
        let rows = self.rows();
        let muted = theme::color(cx, "fg-muted");
```

with

```rust
        let rows = self.rows();
        let options = date_options(cx);
        let muted = theme::color(cx, "fg-muted");
```

and replace

```rust
                let detail = if shelf {
                    let count = self.shelves.iter().find(|entry| entry.id == row.key).map_or(0, |e| e.files.len());
                    file_count(count, &mut cx.default_global::<Localization>().0)
                } else {
```

with

```rust
                let detail = if shelf {
                    let entry = self.shelves.iter().find(|entry| entry.id == row.key);
                    let (files, time) = entry.map_or((0, 0.0), |entry| (entry.files.len(), entry.time_ms));
                    shelf_detail(files, time, self.clock, &options, &mut cx.default_global::<Localization>().0)
                } else {
```

The existing test `preserves_file_targets_through_selection_menus_toolbar_and_activation` keeps passing: its shelf has `time_ms: 0.0`, which shows no time.

### C3. Error log sink: what is built and what is designed

Built: `platform/error_log_sink.rs` writes a window's failures as the webview's `installErrorLogging` does (`[label] kind: description`, once per repeat, at most 50 per window, via `platform::misc::error_log::ErrorReporter`), under the log target `frontend` like `log_frontend` (`src-tauri/src/commands/app.rs:305-311`), and `log_error(context, reason)` like `logError` (`src/platform/errorLog.ts:22-25`).

Designed, not wired (owner decision needed; nothing here edits a compiled file):

- The native app has no "uncaught error" or "unhandled rejection" event. The nearest counterparts are a failing action handler (`platform::actions::execute_action`'s `ActionError::Failed` arm, which logs today) and a detached task's error. Hypothesis: the counterpart of "nothing caught it" is the failed action handler; check with the owner. If accepted, the hunk in `platform/actions.rs` is, in `pub fn execute_action`, after `tracing::error!("action {id} failed: {error:#}");`:

  ```rust
            let label = crate::workbench::for_window(window, cx).map(|w| w.read(cx).label().to_owned());
            super::error_log_sink::report_unhandled(label.as_deref().unwrap_or("?"), &error, cx);
  ```

  and `forget_window(label, cx)` where a workbench window closes (`workbench/windows.rs`).

- The log file: the webview's log goes to a daily file (`src-tauri/src/logging.rs`, `tracing_appender` + `dirs`, `~/Library/Logs/<identifier>`). `app-gpui/Cargo.toml` has neither `tracing-appender` nor `dirs`, and `main.rs` logs to stderr only. A file layer and `help.showLog` (`appLogLocation`) need a new dependency or a hand-written writer, plus the app identifier: not contained, not built.
- Panics: `logging.rs` writes a panic to the file synchronously from a panic hook. Natively that belongs with the file sink above.

### Compile risks (C)

- `shelf_time.rs` calls `settings::global(cx).general.date_format` as `log_view.rs` does; `date_format_options(serde_json::to_value(...).ok().as_ref())` as there.
- `error_log_sink.rs` tests use `tracing_subscriber::fmt().without_time()`; drop it if the builder lacks it in the locked version (then the split on `"frontend: "` still works).

### Tests most likely to fail first (C)

- `a_shelf_row_shows_its_files_then_when_it_was_shelved`: the exact English of `Timestamp::localized` (`"5 min ago"` from `tx_built(&["", " min ago"], Number(5.0))`) assumes a number prints without decimals.
- `writes_uncaught_errors_and_unhandled_rejections_once_each_with_the_window`: `set_default` scopes the subscriber to this thread; GPUI tests run on it.

### What the webview does that is not built (C)

- The shelf's tooltip and the shelf's branch are not shown (not in the webview row either; `src/features/vcs/shelf.tsx:533-539` shows the icon, name, file count and `· time`).

### `parity.json` (C)

| Key                                  | Test                                                                                                                                                                             | Status after integration                                            |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| `action:vcs.shelve`                  | `vcs::shelf_time::tests::the_shelf_tab_shows_each_shelfs_time_after_its_files`, `…::a_shelf_row_shows_its_files_then_when_it_was_shelved`, existing `workbench::shelf::tests::…` | done                                                                |
| `action:vcs.showShelf`               | same                                                                                                                                                                             | done                                                                |
| `module:src/platform/errorLog.ts`    | `platform::error_log_sink::tests::writes_uncaught_errors_and_unhandled_rejections_once_each_with_the_window`, `…::a_window_writes_at_most_its_share_of_failures`                 | partial: no native event reports to it until C3's design is decided |
| `unit:src/platform/errorLog.test.ts` | same                                                                                                                                                                             | partial: the sink is tested; window events are not wired            |

## Source lines of every user-visible string

- `"Current position "`, `"No active editor"`, `"Go to Line:Column"`, `"line[:column]"`: `src/editor/core/view/GotoLineDialog.tsx` (`tx\`Current position ${…}:${…}\``, `tx("No active editor")`, `title="Go to Line:Column"`, `placeholder="line[:column]"`); already in `editor/goto_line.rs`.
- `" · "` between the file count and the time: `src/features/vcs/shelf.tsx:538` (``{n.entry.timeMs ? ` · ${formatTime(n.entry.timeMs)}` : ""}``).
- The time text: `src/features/vcs/model.ts:116` (`formatTime` → `formatTimestamp`), already `vcs_diff::model::format_time`.
- `"uncaught error"`, `"unhandled rejection"`, `[label]`: `src/platform/errorLog.ts:41-46`, already `platform::misc::error_log`.
- No new string is added by pieces A and B.

## IPC commands the actions stand for

- `edit.*`: none (`document.execCommand` / CodeMirror commands in the webview).
- `navigate.gotoLine`: none (`editorController().openFile` on the open document).
- `vcs.shelve` / `vcs.showShelf` rows: the shelf list comes from `vcs_shelf_list` as today; no new call.
- Error log: `log_frontend` (`src-tauri/src/commands/app.rs:305`), whose effect (`tracing::error!(target: "frontend", …)`) is called directly.
