//! Where the window's popups open, and Go to Line:Column as a dialog.
//!
//! A popup that acts on the caret (Show Usages, the bookmark mnemonic chooser) opens under the
//! caret's line instead of near the top of the window; [`crate::ui::popup_anchor`] says where. The
//! popup slot then draws it at that point, kept inside the window.
//!
//! Go to Line:Column is a modal dialog in the middle of the window: a click beside it does not close
//! it, Escape and Cancel do, and the keyboard goes back to what had it. Enter puts the caret of the
//! editor at the typed `line[:column]` and gives the editor the keyboard.

use super::{Popup, Workbench};
use crate::editor::goto_line::{GotoLine, GotoLineEvent};
use crate::editor::view::Editor;
use gpui_kit::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Pixels, Point, Render, Size, Subscription, Window, div,
    prelude::*,
};

/// Go to Line:Column while it is open.
pub(super) struct GotoLineDialog {
    view: Entity<GotoLine>,
    /// What had the keyboard when it opened.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

/// A place for a popup that acts on the editor: a function of [`crate::ui::popup_anchor`].
pub(super) type EditorPlace = fn(Option<&Editor>, Size<Pixels>, &App) -> Point<Pixels>;

impl Workbench {
    /// Opens `view` in the popup slot with its top left corner at `at` (moved to stay inside the
    /// window), as [`Workbench::open_popup`] opens one in the slot's usual place.
    pub(super) fn open_popup_at<V: Focusable + Render>(
        &mut self,
        view: &Entity<V>,
        events: Subscription,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_popup(view, events, window, cx);
        if let Some(popup) = self.popup.as_mut() {
            popup.at = Some(at);
        }
    }

    /// Where a popup acting on the active editor goes, by `place`.
    pub(super) fn editor_popup_at(&self, place: EditorPlace, window: &Window, cx: &App) -> Point<Pixels> {
        place(self.editor().map(|editor| editor.read(cx)), window.viewport_size(), cx)
    }

    /// Where the Select In chooser for `path` goes: under the caret when `path` is the edited file.
    pub(super) fn select_in_popup_at(&self, path: &str, window: &Window, cx: &App) -> Point<Pixels> {
        let editor =
            self.editor().map(|editor| editor.read(cx)).filter(|editor| editor.path().to_string_lossy() == path);
        crate::ui::popup_anchor::select_in_chooser(editor, window.viewport_size(), cx)
    }

    /// What the popup slot draws for `popup`: the popup in the slot's usual place, or anchored at its
    /// point. A press inside it does not reach the backdrop that closes it.
    pub(super) fn popup_body(popup: &Popup) -> AnyElement {
        let body = div()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(popup.view.clone());
        match popup.at {
            Some(at) => gpui_kit::anchored()
                .position(at)
                .snap_to_window()
                .child(body.debug_selector(|| "popup-anchored".to_owned()))
                .into_any_element(),
            None => body.into_any_element(),
        }
    }

    /// Opens Go to Line:Column for the active editor, on its caret's position. Without an editor it
    /// says so and goes nowhere.
    pub(super) fn open_goto_line_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_popup(window, cx);
        let restore = match self.goto_line.take() {
            // Asked for again while open: the new dialog gives the keyboard back where the first would.
            Some(open) => open.restore,
            None => window.focused(cx),
        };
        let position = self.editor().map(|editor| editor.read(cx).caret_position());
        let view = cx.new(|cx| GotoLine::new(position, cx));
        let events = cx.subscribe_in(&view, window, |workbench, _, event: &GotoLineEvent, window, cx| match *event {
            GotoLineEvent::Dismissed => workbench.close_goto_line_dialog(window, cx),
            GotoLineEvent::Go { line, col } => {
                workbench.goto_line = None;
                let Some(editor) = workbench.editor().cloned() else {
                    workbench.publish_context(window, cx);
                    return;
                };
                editor.update(cx, |editor, cx| {
                    // The line and the column are held inside the document.
                    let at = editor.offset_of(line, col);
                    editor.select(at, at, cx);
                    editor.scroll_to_line(line.saturating_sub(5), cx);
                });
                window.focus(&editor.focus_handle(cx), cx);
                workbench.publish_context(window, cx);
                cx.notify();
            }
        });
        window.focus(&view.focus_handle(cx), cx);
        self.goto_line = Some(GotoLineDialog { view, restore, _events: events });
        self.publish_context(window, cx);
        cx.notify();
    }

    /// Closes Go to Line:Column without going anywhere; the keyboard goes back to what had it, or to
    /// the editor.
    pub(super) fn close_goto_line_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.goto_line.take() else { return };
        if let Some(target) = dialog.restore.or_else(|| self.editor().map(|editor| editor.focus_handle(cx))) {
            window.focus(&target, cx);
        }
        self.publish_context(window, cx);
        cx.notify();
    }

    /// Go to Line:Column, while it is open.
    pub fn goto_line_dialog(&self) -> Option<&Entity<GotoLine>> {
        self.goto_line.as_ref().map(|dialog| &dialog.view)
    }

    /// The dialog's layer: over the window, which it keeps from the pointer, with the dialog in the
    /// middle. A click beside it changes nothing.
    pub(super) fn goto_line_layer(&self) -> Option<AnyElement> {
        let view = self.goto_line.as_ref()?.view.clone();
        let layer = div()
            .id("goto-line-layer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .child(div().debug_selector(|| "goto-line-dialog".to_owned()).child(view));
        Some(gpui_kit::deferred(layer).with_priority(1).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{run_action, search_project, start, wait_long};
    use super::*;
    use crate::everywhere::bookmarks_popup::MnemonicChooser;
    use crate::platform::actions::ActionRegistry;
    use gpui_kit::{Bounds, Modifiers, TestAppContext, VisualTestContext, point, px};
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    fn close_project(cx: &mut VisualTestContext) {
        let host = cx.update(|_, cx| cx.global::<crate::backend::Backend>().host.clone());
        host.close_project("w1");
    }

    /// Opens `file`, gives its editor the keyboard and puts the caret at `line`, `col`.
    fn edit_at(workbench: &Entity<Workbench>, file: &Path, line: u32, col: u32, cx: &mut VisualTestContext) {
        let at = file.to_path_buf();
        workbench.update_in(cx, |w, window, cx| w.open_project_file(at, window, cx));
        wait_long(cx, "the file to open", |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == file))
        });
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        cx.update(|window, cx| {
            window.activate_window();
            editor.update(cx, |editor, cx| {
                let offset = editor.offset_of(line, col);
                editor.select(offset, offset, cx);
            });
            let focus = editor.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
    }

    /// The bottom left corner of the caret's line, as the editor painted it.
    fn caret_bottom(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Point<Pixels> {
        cx.run_until_parked();
        workbench.read_with(cx, |w, cx| {
            let editor = w.editor().expect("an editor").read(cx);
            let top = editor.position_of(editor.caret()).expect("the caret's line is painted");
            point(top.x, top.y + editor.line_height(cx))
        })
    }

    fn anchored_bounds(cx: &mut VisualTestContext) -> Bounds<Pixels> {
        cx.run_until_parked();
        cx.debug_bounds("popup-anchored").expect("the popup is drawn at its point")
    }

    fn editor_has_keyboard(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| {
            let editor = workbench.read(cx).editor().cloned().expect("an editor");
            editor.focus_handle(cx).is_focused(window)
        })
    }

    fn caret(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> (u32, u32) {
        workbench.read_with(cx, |w, cx| w.editor().expect("an editor").read(cx).caret_position())
    }

    /// The actions the keyboard runs while `keys` are pressed.
    fn actions_run_by(keys: &str, cx: &mut VisualTestContext) -> Vec<String> {
        let ran: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = ran.clone();
        let listener = cx.update(|_, cx| {
            cx.global_mut::<ActionRegistry>().on_action_executed(move |id, _, _, _| sink.borrow_mut().push(id.into()))
        });
        cx.simulate_keystrokes(keys);
        cx.run_until_parked();
        cx.update(|_, cx| cx.global_mut::<ActionRegistry>().remove_listener(listener));
        ran.take()
    }

    fn goto_line_key() -> &'static str {
        if cfg!(target_os = "macos") { "cmd-l" } else { "ctrl-g" }
    }

    /// Go to Line:Column opens as a dialog on the caret's position with the position under the
    /// field; a click beside it leaves it open; Enter on `line:column` moves the caret there and
    /// gives the editor the keyboard.
    #[gpui_kit::test]
    async fn go_to_line_is_a_dialog_and_enter_moves_the_caret_to_the_typed_line_and_column(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        edit_at(&workbench, &root.join("src/alpha.rs"), 0, 4, cx);

        assert_eq!(actions_run_by(goto_line_key(), cx), ["navigate.gotoLine"]);
        let dialog = workbench.read_with(cx, |w, _| w.goto_line_dialog().cloned()).expect("the dialog is open");
        assert!(workbench.read_with(cx, |w, _| w.popup.is_none()), "it is not a popup of the popup slot");
        assert_eq!(dialog.update(cx, |dialog, cx| dialog.hint(cx)), "Current position 1:5");
        let typed = dialog.read_with(cx, |dialog, cx| dialog.field().read(cx).text().to_owned());
        assert_eq!(typed, "1:5");

        // A click beside the dialog does not close it.
        cx.run_until_parked();
        let shown = cx.debug_bounds("goto-line-dialog").expect("the dialog is drawn");
        let beside = point(shown.left() - px(20.0), shown.top() - px(20.0));
        cx.simulate_click(beside, Modifiers::default());
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.goto_line_dialog().is_some()), "a click beside it keeps it");
        cx.update(|window, cx| {
            let focus = dialog.focus_handle(cx);
            window.focus(&focus, cx);
        });

        cx.simulate_input("3:5");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(caret(&workbench, cx), (2, 4));
        assert!(workbench.read_with(cx, |w, _| w.goto_line_dialog().is_none()), "Enter closes the dialog");
        assert!(editor_has_keyboard(&workbench, cx), "the editor has the keyboard");
        close_project(cx);
    }

    /// The column may follow a comma, and a line alone goes to its first column.
    #[gpui_kit::test]
    async fn go_to_line_takes_a_line_a_line_and_column_and_a_comma(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        edit_at(&workbench, &root.join("src/alpha.rs"), 0, 0, cx);

        run_action(cx, "navigate.gotoLine", None);
        cx.simulate_input("4,3");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(caret(&workbench, cx), (3, 2));

        run_action(cx, "navigate.gotoLine", None);
        cx.simulate_input("2");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(caret(&workbench, cx), (1, 0));

        // Text that names no position leaves the dialog open and the caret where it is.
        run_action(cx, "navigate.gotoLine", None);
        cx.simulate_input("0:1");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.goto_line_dialog().is_some()));
        assert_eq!(caret(&workbench, cx), (1, 0));
        close_project(cx);
    }

    /// Escape closes the dialog without moving the caret and gives the keyboard back to the editor.
    #[gpui_kit::test]
    async fn escape_closes_go_to_line_and_the_editor_has_the_keyboard_again(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        edit_at(&workbench, &root.join("src/alpha.rs"), 2, 3, cx);

        run_action(cx, "navigate.gotoLine", None);
        assert!(!editor_has_keyboard(&workbench, cx), "the dialog's field has the keyboard");
        cx.simulate_input("5");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.goto_line_dialog().is_none()));
        assert_eq!(caret(&workbench, cx), (2, 3));
        assert!(editor_has_keyboard(&workbench, cx));
        close_project(cx);
    }

    /// The key of Go to Line:Column runs nothing while a tool window has the keyboard.
    #[gpui_kit::test]
    async fn the_go_to_line_key_runs_nothing_while_a_tool_window_has_the_keyboard(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        edit_at(&workbench, &root.join("src/alpha.rs"), 0, 0, cx);
        let tree = workbench.read_with(cx, |w, _| w.project_tree().cloned()).expect("the Project view");
        cx.update(|window, cx| {
            let focus = tree.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        assert_eq!(actions_run_by(goto_line_key(), cx), Vec::<String>::new());
        assert!(workbench.read_with(cx, |w, _| w.goto_line_dialog().is_none()));
        close_project(cx);
    }

    /// Show Usages opens under the caret's line, with the popup's gap.
    #[gpui_kit::test]
    async fn show_usages_opens_under_the_caret(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        // On `Widget` of `pub struct Widget;`.
        edit_at(&workbench, &root.join("src/alpha.rs"), 0, 13, cx);
        let caret = caret_bottom(&workbench, cx);

        run_action(cx, "find.showUsages", None);
        wait_long(cx, "the usages popup", |cx| workbench.read_with(cx, |w, _| w.usages_popup().is_some()));
        let bounds = anchored_bounds(cx);
        assert_eq!(bounds.origin, point(caret.x, caret.y + px(4.0)));
        close_project(cx);
    }

    /// The mnemonic chooser opens just under the caret's line.
    #[gpui_kit::test]
    async fn the_mnemonic_chooser_opens_under_the_caret(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        edit_at(&workbench, &root.join("src/alpha.rs"), 2, 7, cx);
        let caret = caret_bottom(&workbench, cx);

        run_action(cx, "navigate.toggleBookmarkWithMnemonic", None);
        let chooser = workbench.read_with(cx, |w, _| {
            w.popup.as_ref().and_then(|popup| popup.view.clone().downcast::<MnemonicChooser>().ok())
        });
        assert!(chooser.is_some(), "the chooser is open");
        let bounds = anchored_bounds(cx);
        assert_eq!(bounds.origin, point(caret.x, caret.y + px(6.0)));
        close_project(cx);
    }

    /// Select In for the edited file opens its chooser just under the caret's line.
    #[gpui_kit::test]
    async fn select_in_opens_its_chooser_under_the_caret_of_the_edited_file(cx: &mut TestAppContext) {
        use crate::project_view::context_menu::ProjectCommand;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        edit_at(&workbench, &root.join("src/alpha.rs"), 2, 0, cx);
        let caret = caret_bottom(&workbench, cx);

        run_action(cx, "navigate.selectIn", None);
        assert!(workbench.read_with(cx, |w, _| w.open_menu::<ProjectCommand>()).is_some(), "the chooser is open");
        cx.run_until_parked();
        let row = cx.debug_bounds("menu-row-0-0").expect("the chooser's first row is drawn");
        // The first row sits inside the menu's frame, whose corner is the anchor.
        let (dx, dy) = (row.left() - caret.x, row.top() - (caret.y + px(2.0)));
        assert!(dx >= px(0.0) && dx <= px(8.0), "the chooser starts at the caret's left: {dx:?}");
        assert!(dy >= px(0.0) && dy <= px(8.0), "the chooser starts under the caret's line: {dy:?}");
        close_project(cx);
    }
}
