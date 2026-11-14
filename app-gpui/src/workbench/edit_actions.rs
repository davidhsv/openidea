//! Edit › Undo, Redo, Cut, Copy, Paste and Select All act on what has the keyboard. In the code
//! editor they keep the editor's own handling (its history holds programmatic edits too); in a text
//! field, a list or a tree that answers for the Edit commands ([`crate::platform::edit_target`])
//! they act on that element, and the editor behind it is left alone.
//!
//! The text fields of dialogs, popups and tool windows edit, undo, copy and paste their own text;
//! a copy goes into the clipboard history of Paste from History. A secret field is never copied or
//! cut. The Project view takes Undo for its last file operation and leaves the other commands
//! without effect; the Services tree selects every row on Select All.

use crate::app_models::edit_command::{EditCommand, EditFocus, EditRoute, route_edit_command};
use crate::platform::actions::{Action, ActionCall, ActionRegistry};
use crate::platform::edit_target::{self, EditTarget};
use crate::project_tree::ProjectTree;
use crate::services::view::ServicesView;
use crate::ui::secret_field::SecretField;
use crate::ui::text_area::TextArea;
use crate::ui::text_field::TextField;
use gpui_kit::{App, Context, Focusable, Modifiers, Window};

/// The Edit actions and the command each one is.
const EDIT_ACTIONS: [(&str, EditCommand); 6] = [
    ("edit.undo", EditCommand::Undo),
    ("edit.redo", EditCommand::Redo),
    ("edit.cut", EditCommand::Cut),
    ("edit.copy", EditCommand::Copy),
    ("edit.paste", EditCommand::Paste),
    ("edit.selectAll", EditCommand::SelectAll),
];

/// The command of the Edit action `id`.
fn command_of(id: &str) -> Option<EditCommand> {
    EDIT_ACTIONS.iter().find(|(action, _)| *action == id).map(|(_, command)| *command)
}

/// Puts the Edit actions in front of the editor's handlers of the same ids, which run when nothing
/// that answers for the Edit commands has the keyboard. Runs after [`crate::editor::actions::register`].
pub(super) fn register_actions(registry: &mut ActionRegistry) {
    for (id, command) in EDIT_ACTIONS {
        let Some(previous) = registry.get(id) else { continue };
        let meta = previous.meta.clone();
        registry.register(Action::new(meta, move |call, window, cx| run(command, &previous, call, window, cx)));
    }
}

/// Where the keyboard is, as the routing of the Edit commands sees it.
fn edit_focus(window: &Window, cx: &App) -> EditFocus {
    let Some(editor) = super::active_editor(window, cx) else { return EditFocus::default() };
    let focus = editor.focus_handle(cx);
    EditFocus { in_editor_area: focus.contains_focused(window, cx), editor: focus.is_focused(window) }
}

fn run(
    command: EditCommand,
    previous: &Action,
    call: &ActionCall,
    window: &mut Window,
    cx: &mut App,
) -> anyhow::Result<()> {
    // Documents keep no history apart from their editor's: the editor's chain answers for them.
    match route_edit_command(command, edit_focus(window, cx), |_| false) {
        EditRoute::DocumentHistory | EditRoute::EditorHistory => previous.run_handler(call, window, cx),
        EditRoute::Focused => match edit_target::edit(command, window, cx) {
            Some(_) => Ok(()),
            None => previous.run_handler(call, window, cx),
        },
    }
}

/// Whether the Edit menu greys out the item of action `id`: an Edit command the element with the
/// keyboard cannot do now.
pub(super) fn edit_item_disabled(id: &str, window: &Window, cx: &App) -> bool {
    command_of(id).is_some_and(|command| edit_target::can_edit(command, window, cx) == Some(false))
}

impl EditTarget for TextField {
    fn can_edit(&self, command: EditCommand, cx: &App) -> bool {
        let selected = !self.selection().is_empty();
        match command {
            EditCommand::Undo => self.can_undo(),
            EditCommand::Redo => self.can_redo(),
            EditCommand::Cut | EditCommand::Copy => selected,
            EditCommand::Paste => {
                !self.is_read_only() && cx.read_from_clipboard().and_then(|item| item.text()).is_some()
            }
            EditCommand::SelectAll => !self.text().is_empty(),
        }
    }

    fn edit(&mut self, command: EditCommand, _: &mut Window, cx: &mut Context<Self>) -> bool {
        match command {
            EditCommand::Undo => self.undo(cx),
            EditCommand::Redo => self.redo(cx),
            EditCommand::Cut => self.cut(cx),
            EditCommand::Copy => self.copy(cx),
            EditCommand::Paste => self.paste(cx),
            EditCommand::SelectAll => {
                self.select_all(cx);
                true
            }
        }
    }
}

/// The multi-line fields (the commit message): the same commands on their text.
impl EditTarget for TextArea {
    fn can_edit(&self, command: EditCommand, cx: &App) -> bool {
        let selected = !self.selection().is_empty();
        let editable = !self.is_read_only() && !self.is_disabled();
        match command {
            EditCommand::Undo => self.can_undo(),
            EditCommand::Redo => self.can_redo(),
            EditCommand::Cut => selected && editable,
            EditCommand::Copy => selected,
            EditCommand::Paste => editable && cx.read_from_clipboard().and_then(|item| item.text()).is_some(),
            EditCommand::SelectAll => !self.text().is_empty(),
        }
    }

    fn edit(&mut self, command: EditCommand, _: &mut Window, cx: &mut Context<Self>) -> bool {
        match command {
            EditCommand::Undo => {
                let could = self.can_undo();
                self.undo(cx);
                could
            }
            EditCommand::Redo => {
                let could = self.can_redo();
                self.redo(cx);
                could
            }
            EditCommand::Cut => self.cut(cx),
            EditCommand::Copy => self.copy(cx),
            EditCommand::Paste => self.paste(cx),
            EditCommand::SelectAll => {
                self.select_all(cx);
                true
            }
        }
    }
}

/// A secret is never put on the clipboard: Copy and Cut are taken and do nothing.
impl EditTarget for SecretField {
    fn can_edit(&self, command: EditCommand, cx: &App) -> bool {
        match command {
            EditCommand::Cut | EditCommand::Copy => false,
            _ => self.state().read(cx).can_edit(command, cx),
        }
    }

    fn edit(&mut self, command: EditCommand, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match command {
            EditCommand::Cut | EditCommand::Copy => false,
            _ => {
                let state = self.state().clone();
                let changed = state.update(cx, |state, cx| state.edit(command, window, cx));
                cx.notify();
                changed
            }
        }
    }
}

/// The Project view: Undo brings back what its last file operation changed; the other commands have
/// nothing to act on in it.
impl EditTarget for ProjectTree {
    fn can_edit(&self, command: EditCommand, _: &App) -> bool {
        command == EditCommand::Undo
    }

    fn edit(&mut self, command: EditCommand, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if command != EditCommand::Undo {
            return false;
        }
        // The undo updates this tree: it runs once this update is over.
        window.defer(cx, |window, cx| {
            super::undo_in_project_view(window, cx);
        });
        true
    }
}

/// The Services tree: Select All selects every row; the other commands have nothing to act on.
impl EditTarget for ServicesView {
    fn can_edit(&self, command: EditCommand, _: &App) -> bool {
        command == EditCommand::SelectAll && !self.rows().is_empty()
    }

    fn edit(&mut self, command: EditCommand, _: &mut Window, cx: &mut Context<Self>) -> bool {
        if command != EditCommand::SelectAll {
            return false;
        }
        let rows = self.rows();
        let (Some(first), Some(last)) = (rows.first(), rows.last()) else { return false };
        let (first, last) = (first.key.clone(), last.key.clone());
        self.click(&first, Modifiers::default(), cx);
        self.click(&last, Modifiers { shift: true, ..Modifiers::default() }, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::Workbench;
    use super::super::tests::{git_in, git_project, init_repo, run_action, search_project, start, wait_long};
    use super::*;
    use crate::editor::view::EditorClipboard;
    use crate::platform::actions::execute_action_or_throw;
    use crate::services::model::{ItemAction, NodeCommand, ServiceMenuEntry, ServiceNode};
    use crate::services::view::{ServicesBanner, ServicesSource};
    use gpui_kit::ClipboardItem;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};
    use ide_core::TextChange;
    use std::path::Path;
    use std::rc::Rc;

    fn primary(key: &str) -> String {
        if cfg!(target_os = "macos") { format!("cmd-{key}") } else { format!("ctrl-{key}") }
    }

    fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    fn set_clipboard(cx: &mut VisualTestContext, text: &str) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
    }

    fn run(cx: &mut VisualTestContext, id: &str) {
        cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).unwrap_or_else(|e| panic!("{id}: {e}"));
        cx.run_until_parked();
    }

    fn text_of(field: &Entity<TextField>, cx: &mut VisualTestContext) -> String {
        field.read_with(cx, |field, _| field.text().to_owned())
    }

    fn selected_of(field: &Entity<TextField>, cx: &mut VisualTestContext) -> String {
        field.read_with(cx, |field, _| field.selected_text().to_owned())
    }

    fn history_top(cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| {
            cx.try_global::<EditorClipboard>().and_then(|history| history.0.entries().first().map(|e| e.text.clone()))
        })
    }

    /// Whether the Edit menu greys out `id`: the rule the menu bar applies, and on Windows and Linux,
    /// where the Edit items are the actions themselves, the menu state as built.
    fn greyed(cx: &mut VisualTestContext, id: &str) -> bool {
        let greyed = cx.update(|window, cx| edit_item_disabled(id, window, cx));
        if !cfg!(target_os = "macos") {
            let in_menu = cx.update(|window, cx| super::super::menu::state(window, cx).disabled.contains(id));
            assert_eq!(in_menu, greyed, "the menu bar shows {id} as the rule says");
        }
        greyed
    }

    fn focus<V: Focusable>(view: &Entity<V>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.activate_window();
            let focus = view.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
    }

    fn close_project(cx: &mut VisualTestContext) {
        let host = cx.update(|_, cx| cx.global::<crate::backend::Backend>().host.clone());
        host.close_project("w1");
    }

    fn open_editor(
        workbench: &Entity<Workbench>,
        file: &Path,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::editor::view::Editor> {
        let at = file.to_path_buf();
        workbench.update_in(cx, |w, window, cx| w.open_project_file(at, window, cx));
        wait_long(cx, "the file to open", |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == file))
        });
        workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor")
    }

    // src/features/app/__tests__/editCommand.test.ts › Edit menu Undo / Redo › use the focused editor's history, including programmatic edits
    #[gpui_kit::test]
    async fn use_the_focused_editors_history_including_programmatic_edits(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let editor = open_editor(&workbench, &root.join("src/beta.rs"), cx);
        focus(&editor, cx);
        let original = editor.read_with(cx, |e, _| e.text());
        let change = TextChange { from: 0, to: 0, insert: "// note\n".to_owned() };
        editor.update(cx, |editor, cx| editor.apply_edit(&[change], vec![(0, 0)], 0, cx));
        let edited = format!("// note\n{original}");
        wait_long(cx, "the programmatic edit", |cx| editor.read_with(cx, |e, _| e.text()) == edited);

        run(cx, "edit.undo");
        wait_long(cx, "the undo", |cx| editor.read_with(cx, |e, _| e.text()) == original);
        run(cx, "edit.redo");
        wait_long(cx, "the redo", |cx| editor.read_with(cx, |e, _| e.text()) == edited);

        // Select All and Copy still act on the editor.
        set_clipboard(cx, "before");
        run(cx, "edit.selectAll");
        run(cx, "edit.copy");
        assert_eq!(clipboard(cx).as_deref(), Some(edited.as_str()));
        close_project(cx);
    }

    // A dialog's text field: every Edit command through the registry and through its key acts on the
    // field; the Edit menu greys out what the field cannot do; the editor is left alone.
    #[gpui_kit::test]
    async fn the_edit_commands_act_on_the_text_field_of_a_dialog(cx: &mut TestAppContext) {
        let (workbench, cx, _dirs, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["checkout", "-q", "-b", "topic"]);
        });
        std::fs::write(root.join("kept.txt"), "kept\n").expect("the file is written");
        let editor = open_editor(&workbench, &root.join("kept.txt"), cx);

        run_action(cx, "vcs.newBranch", None);
        wait_long(cx, "the dialog", |cx| workbench.read_with(cx, |w, _| w.vcs_prompt().is_some()));
        let dialog = workbench.read_with(cx, |w, _| w.vcs_prompt()).expect("the dialog");
        let field = dialog.read_with(cx, |dialog, _| dialog.field().cloned()).expect("the dialog's field");
        focus(&field, cx);
        let start_text = text_of(&field, cx);
        assert!(!start_text.is_empty(), "the field suggests a name");

        // Through the registry.
        set_clipboard(cx, "before");
        field.update(cx, |field, cx| field.select_range(0..0, cx));
        assert!(greyed(cx, "edit.copy") && greyed(cx, "edit.cut"), "nothing is selected");
        assert!(!greyed(cx, "edit.paste") && !greyed(cx, "edit.selectAll"));
        run(cx, "edit.selectAll");
        assert_eq!(selected_of(&field, cx), start_text);
        assert!(!greyed(cx, "edit.copy"));
        run(cx, "edit.copy");
        assert_eq!(clipboard(cx), Some(start_text.clone()));
        assert_eq!(history_top(cx), Some(start_text.clone()), "the copy is in the clipboard history");
        run(cx, "edit.cut");
        assert_eq!(text_of(&field, cx), "");
        assert!(!greyed(cx, "edit.undo") && greyed(cx, "edit.redo"));
        run(cx, "edit.undo");
        assert_eq!(text_of(&field, cx), start_text);
        run(cx, "edit.redo");
        assert_eq!(text_of(&field, cx), "");
        set_clipboard(cx, "pasted-name");
        run(cx, "edit.paste");
        assert_eq!(text_of(&field, cx), "pasted-name");

        // Through the keys.
        cx.simulate_keystrokes(&primary("a"));
        assert_eq!(selected_of(&field, cx), "pasted-name");
        cx.simulate_keystrokes(&primary("c"));
        assert_eq!(clipboard(cx).as_deref(), Some("pasted-name"));
        cx.simulate_keystrokes(&primary("x"));
        assert_eq!(text_of(&field, cx), "");
        cx.simulate_keystrokes(&primary("z"));
        assert_eq!(text_of(&field, cx), "pasted-name");
        cx.simulate_keystrokes(&primary("shift-z"));
        assert_eq!(text_of(&field, cx), "");
        cx.simulate_keystrokes(&primary("v"));
        assert_eq!(text_of(&field, cx), "pasted-name");

        assert_eq!(editor.read_with(cx, |e, _| e.text()), "kept\n", "the editor behind the dialog is untouched");
        close_project(cx);
    }

    // src/editor/core/clipboardHistory.test.ts › clipboard history (Paste from History) › outside editors › records the selected text of inputs
    #[gpui_kit::test]
    async fn records_the_selected_text_of_inputs(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, _root, workbench, cx) = search_project(cx);
        run_action(cx, "find.inPath", None);
        let popup = workbench.read_with(cx, |w, _| w.find_popup()).expect("Find in Files is open");
        let field = popup.read_with(cx, |popup, _| popup.pattern_field().clone());
        focus(&field, cx);
        field.update(cx, |field, cx| {
            field.set_text("hello world", cx);
            field.select_range(6..11, cx);
        });
        run(cx, "edit.copy");
        assert_eq!(history_top(cx).as_deref(), Some("world"));
        assert_eq!(clipboard(cx).as_deref(), Some("world"));
        close_project(cx);
    }

    // Find in Files: typing, Undo and Redo in its field; the search follows the text.
    #[gpui_kit::test]
    async fn the_edit_commands_act_on_the_find_in_files_field(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, _root, workbench, cx) = search_project(cx);
        run_action(cx, "find.inPath", None);
        let popup = workbench.read_with(cx, |w, _| w.find_popup()).expect("Find in Files is open");
        let field = popup.read_with(cx, |popup, _| popup.pattern_field().clone());
        focus(&field, cx);
        run(cx, "edit.selectAll");
        cx.simulate_input("needle");
        assert_eq!(text_of(&field, cx), "needle");
        run(cx, "edit.undo");
        assert_eq!(text_of(&field, cx), "", "the typed run goes in one step");
        run(cx, "edit.redo");
        assert_eq!(text_of(&field, cx), "needle");
        cx.simulate_keystrokes(&primary("a"));
        run(cx, "edit.cut");
        assert_eq!((text_of(&field, cx), clipboard(cx)), (String::new(), Some("needle".to_owned())));
        cx.simulate_keystrokes(&primary("v"));
        assert_eq!(text_of(&field, cx), "needle");
        assert!(workbench.read_with(cx, |w, _| w.find_popup().is_some()), "the popup stays open");
        close_project(cx);
    }

    // The Project view: Copy, Cut, Paste, Select All and Redo leave the editor and the clipboard as
    // they were and are greyed out in the Edit menu; Undo stays available for its file operations.
    #[gpui_kit::test]
    async fn in_the_project_view_the_edit_commands_leave_the_editor_alone(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let editor = open_editor(&workbench, &root.join("src/beta.rs"), cx);
        focus(&editor, cx);
        run(cx, "edit.selectAll");
        let text = editor.read_with(cx, |e, _| e.text());
        let tree = workbench.read_with(cx, |w, _| w.project_tree().cloned()).expect("the Project view");
        focus(&tree, cx);
        set_clipboard(cx, "before");
        for id in ["edit.copy", "edit.cut", "edit.paste", "edit.selectAll", "edit.redo"] {
            assert!(greyed(cx, id), "{id} is greyed out");
            run(cx, id);
            assert_eq!(editor.read_with(cx, |e, _| e.text()), text, "{id} leaves the editor");
            assert_eq!(clipboard(cx).as_deref(), Some("before"), "{id} leaves the clipboard");
        }
        assert!(!greyed(cx, "edit.undo"));
        // Without a file operation to undo, Undo does not reach the editor either.
        run(cx, "edit.undo");
        assert_eq!(editor.read_with(cx, |e, _| e.text()), text);
        close_project(cx);
    }

    struct Source;

    impl ServicesSource for Source {
        fn actions(&self, _: &str, _: &str, _: &App) -> Vec<ItemAction<NodeCommand>> {
            Vec::new()
        }

        fn menu(&self, _: &str, _: &str, _: &App) -> Vec<ServiceMenuEntry<NodeCommand>> {
            Vec::new()
        }

        fn can_move(&self, _: &str, _: &str, _: &str, _: &App) -> bool {
            false
        }

        fn add_menu(&self, _: &App) -> Vec<Vec<ServiceMenuEntry<NodeCommand>>> {
            Vec::new()
        }

        fn view_options(&self, _: &App) -> Vec<ServiceMenuEntry<NodeCommand>> {
            Vec::new()
        }

        fn banners(&self, _: &App) -> Vec<ServicesBanner> {
            Vec::new()
        }
    }

    // The Services tree (a multi-selection tree): Select All, through the registry and its key,
    // selects every row; Copy has nothing to copy and leaves the clipboard.
    #[gpui_kit::test]
    async fn select_all_in_the_services_tree_selects_every_row(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (view, cx) = cx.add_window_view(|_, cx| ServicesView::new(Rc::new(Source), cx));
        let leaves = ["one", "two", "three"].map(|key| ServiceNode::leaf(key, key, "run"));
        view.update(cx, |view, cx| view.set_provider_roots("test", 0, leaves.to_vec(), false, cx));
        focus(&view, cx);
        let all = ["one", "two", "three"].map(str::to_owned).to_vec();

        run(cx, "edit.selectAll");
        assert_eq!(view.read_with(cx, |view, _| view.selected_keys()), all);

        view.update(cx, |view, cx| view.click("two", Modifiers::default(), cx));
        assert_eq!(view.read_with(cx, |view, _| view.selected_keys()), ["two"]);
        cx.simulate_keystrokes(&primary("a"));
        assert_eq!(view.read_with(cx, |view, _| view.selected_keys()), all);

        set_clipboard(cx, "before");
        run(cx, "edit.copy");
        assert_eq!(clipboard(cx).as_deref(), Some("before"));
        assert_eq!(cx.update(|window, cx| edit_target::can_edit(EditCommand::Copy, window, cx)), Some(false));
    }

    // A password field is never copied or cut, through the registry or the keys; Paste, Select All
    // and Undo still work in it.
    #[gpui_kit::test]
    async fn a_password_field_is_never_copied_or_cut(cx: &mut TestAppContext) {
        const SECRET: &str = "pw-7f3a";
        let _home = start(cx);
        let (secret, cx) = cx.add_window_view(|_, cx| SecretField::new(cx));
        secret.update(cx, |secret, cx| secret.set_text(SECRET, cx));
        focus(&secret, cx);
        set_clipboard(cx, "before");

        run(cx, "edit.selectAll");
        for id in ["edit.copy", "edit.cut"] {
            run(cx, id);
            assert_eq!(clipboard(cx).as_deref(), Some("before"), "{id}");
        }
        for key in ["c", "x"] {
            cx.simulate_keystrokes(&primary(key));
            assert_eq!(clipboard(cx).as_deref(), Some("before"), "{key}");
        }
        assert_eq!(secret.read_with(cx, |secret, cx| secret.text(cx).to_owned()), SECRET);
        assert_eq!(cx.update(|window, cx| edit_target::can_edit(EditCommand::Copy, window, cx)), Some(false));
        assert_ne!(history_top(cx).as_deref(), Some(SECRET), "the secret is not in the clipboard history");

        run(cx, "edit.paste");
        assert_eq!(secret.read_with(cx, |secret, cx| secret.text(cx).to_owned()), "before");
        run(cx, "edit.undo");
        assert_eq!(secret.read_with(cx, |secret, cx| secret.text(cx).to_owned()), SECRET);
    }
}
