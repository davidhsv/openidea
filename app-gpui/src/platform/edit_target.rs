//! What Edit › Undo, Redo, Cut, Copy, Paste and Select All act on outside the code editor: the
//! element that has the keyboard. A text field, a list or a tree registers its focus handle with
//! the view that answers for it; the Edit actions ask the innermost registered element around the
//! keyboard whether a command applies and have it carry the command out. With none of them around
//! the keyboard the editor's own handling runs, as before.
//!
//! The text fields' undo history lives here too: a field records its text before each edit, a run
//! of typing as one step, as every text input of the platform does.

use crate::app_models::edit_command::EditCommand;
use gpui_kit::{App, Context, FocusHandle, Global, WeakEntity, WeakFocusHandle, Window};
use std::fmt;
use std::rc::Rc;

/// An element the Edit commands can act on.
pub trait EditTarget: 'static {
    /// Whether the element takes `command` at all. One that does not leaves it to the window (the
    /// Project view's Undo restores its last file operation that way).
    fn handles(&self, command: EditCommand) -> bool {
        let _ = command;
        true
    }

    /// Whether `command` would do something now: the Edit menu greys it out otherwise.
    fn can_edit(&self, command: EditCommand, cx: &App) -> bool;

    /// Carries `command` out. Answers whether anything changed (the text, the selection or the
    /// clipboard); a command the element handles but cannot do now does nothing.
    fn edit(&mut self, command: EditCommand, window: &mut Window, cx: &mut Context<Self>) -> bool;
}

type CanFn = dyn Fn(EditCommand, &App) -> Option<bool>;
type RunFn = dyn Fn(EditCommand, &mut Window, &mut App) -> Option<bool>;

struct Entry {
    focus: WeakFocusHandle,
    /// `None` once the element is gone, or for a command it does not handle.
    can: Rc<CanFn>,
    run: Rc<RunFn>,
}

/// The registered elements, newest last. Entries of released elements are dropped as new ones come.
#[derive(Default)]
pub struct EditTargets {
    entries: Vec<Entry>,
}

impl Global for EditTargets {}

/// Has `target` answer the Edit commands while `focus`, or an element inside it that is not
/// registered itself, has the keyboard.
pub fn register<T: EditTarget>(focus: &FocusHandle, target: WeakEntity<T>, cx: &mut App) {
    let read = target.clone();
    let can = move |command: EditCommand, cx: &App| -> Option<bool> {
        let entity = read.upgrade()?;
        let target = entity.read(cx);
        target.handles(command).then(|| target.can_edit(command, cx))
    };
    let run = move |command: EditCommand, window: &mut Window, cx: &mut App| -> Option<bool> {
        let entity = target.upgrade()?;
        entity.update(cx, |target, cx| target.handles(command).then(|| target.edit(command, window, cx)))
    };
    let targets = cx.default_global::<EditTargets>();
    targets.entries.retain(|entry| entry.focus.upgrade().is_some());
    targets.entries.push(Entry { focus: focus.downgrade(), can: Rc::new(can), run: Rc::new(run) });
}

/// The registered element answering for the keyboard: the focused element itself, else the
/// innermost registered element that contains it.
fn focused_entry(window: &Window, cx: &App) -> Option<(Rc<CanFn>, Rc<RunFn>)> {
    let focused = window.focused(cx)?;
    let targets = cx.try_global::<EditTargets>()?;
    let mut around: Option<(FocusHandle, &Entry)> = None;
    for entry in &targets.entries {
        let Some(handle) = entry.focus.upgrade() else { continue };
        if handle == focused {
            return Some((entry.can.clone(), entry.run.clone()));
        }
        if !handle.contains_focused(window, cx) {
            continue;
        }
        // Of two elements around the keyboard, the one inside the other answers.
        let inner = around.as_ref().is_none_or(|(outer, _)| outer.contains(&handle, window));
        if inner {
            around = Some((handle, entry));
        }
    }
    around.map(|(_, entry)| (entry.can.clone(), entry.run.clone()))
}

/// Whether `command` applies to what has the keyboard; `None` when no registered element answers
/// for it (the editor's own state then decides).
pub fn can_edit(command: EditCommand, window: &Window, cx: &App) -> Option<bool> {
    let (can, _) = focused_entry(window, cx)?;
    can(command, cx)
}

/// Carries `command` out on what has the keyboard. `None` when no registered element takes it:
/// the caller then leaves it to the editor. `Some(changed)` otherwise.
pub fn edit(command: EditCommand, window: &mut Window, cx: &mut App) -> Option<bool> {
    let (_, run) = focused_entry(window, cx)?;
    run(command, window, cx)
}

/// The text and selection (byte offsets) of a field at one moment.
#[derive(Clone, PartialEq, Eq)]
pub struct TextState {
    pub text: String,
    pub anchor: usize,
    pub head: usize,
}

/// Names the state by its sizes only: the text may be a secret.
impl fmt::Debug for TextState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextState").field("len", &self.text.len()).field("head", &self.head).finish_non_exhaustive()
    }
}

/// How many steps a field can undo.
pub const MAX_UNDO_STEPS: usize = 100;

/// A text field's undo and redo steps. A run of typed characters is one step; moving the caret,
/// pasting or deleting ends the run.
#[derive(Default)]
pub struct TextHistory {
    undo: Vec<TextState>,
    redo: Vec<TextState>,
    typing: bool,
}

/// Names the history by its sizes only: its states may hold a secret.
impl fmt::Debug for TextHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextHistory").field("undo", &self.undo.len()).field("redo", &self.redo.len()).finish()
    }
}

impl TextHistory {
    /// An edit is about to change `before`. `typing` when it types one character over no
    /// selection: it joins the run of typing before it.
    pub fn record(&mut self, before: TextState, typing: bool) {
        if !(typing && self.typing) {
            self.undo.push(before);
            if self.undo.len() > MAX_UNDO_STEPS {
                self.undo.remove(0);
            }
        }
        self.typing = typing;
        self.redo.clear();
    }

    /// The caret moved or the selection changed: the next character typed starts a new step.
    pub fn break_run(&mut self) {
        self.typing = false;
    }

    /// The owner replaced the text: there is nothing to go back to.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.typing = false;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// The state to go back to from `current`, which Redo then returns to.
    pub fn undo(&mut self, current: TextState) -> Option<TextState> {
        let previous = self.undo.pop()?;
        self.redo.push(current);
        self.typing = false;
        Some(previous)
    }

    /// The state Undo left, from `current`, which Undo then returns to.
    pub fn redo(&mut self, current: TextState) -> Option<TextState> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        self.typing = false;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(text: &str) -> TextState {
        TextState { text: text.to_owned(), anchor: text.len(), head: text.len() }
    }

    // Not from a spec: a run of typing is one step, a paste is its own, and a new edit drops what
    // was undone.
    #[test]
    fn a_run_of_typing_undoes_in_one_step_and_a_new_edit_drops_the_redo_steps() {
        let mut history = TextHistory::default();
        assert!(!history.can_undo() && !history.can_redo());
        history.record(state(""), true);
        history.record(state("a"), true);
        history.record(state("ab"), false);
        assert_eq!(history.undo(state("ab pasted")), Some(state("ab")));
        assert_eq!(history.undo(state("ab")), Some(state("")), "the typed run goes in one step");
        assert_eq!(history.undo(state("")), None);
        assert_eq!(history.redo(state("")), Some(state("ab")));
        assert!(history.can_redo());
        history.record(state("ab"), true);
        assert!(!history.can_redo(), "a new edit drops the steps undone");

        // A caret move between two characters typed makes two steps.
        let mut history = TextHistory::default();
        history.record(state(""), true);
        history.break_run();
        history.record(state("x"), true);
        assert_eq!(history.undo(state("xy")), Some(state("x")));
        history.clear();
        assert!(!history.can_undo() && !history.can_redo());
    }

    // Not from a spec: the history keeps its latest steps and never shows the text it holds.
    #[test]
    fn the_history_is_capped_and_its_debug_output_shows_no_text() {
        let mut history = TextHistory::default();
        for n in 0..MAX_UNDO_STEPS + 5 {
            history.record(state(&n.to_string()), false);
        }
        let mut steps = 0;
        while history.undo(state("now")).is_some() {
            steps += 1;
        }
        assert_eq!(steps, MAX_UNDO_STEPS);
        history.record(state("hunter2"), false);
        assert!(!format!("{history:?} {:?}", state("hunter2")).contains("hunter2"));
    }
}
