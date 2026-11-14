//! Committing from the Commit tool window: the changes tree above, the commit message box below,
//! sharing one commit state (the message, Amend, the options and which files are included).
//!
//! Commit saves the documents, reads the repository again, works out what the inclusion
//! checkboxes take into the commit, checks that something can be committed and commits it on the
//! backend in each repository that has included changes. Afterwards the repository, the tree and
//! the log are read again, the message is kept in the commit message history and the field is left
//! as the "Clear initial commit message" setting says. A failure is reported and keeps the message.
//!
//! Before the commit the before-commit checks run (`shelf::run_commit_checks`); what they find shows
//! in the box and the next Commit, Commit Anyway, skips them.

use super::branches::{This, call, finished, notify, push, refresh, run_action_later, run_op, save_all, vcs_state};
use super::services_window::on_workbench;
use super::{COMMIT_TOOL_WINDOW, GIT_TOOL_WINDOW, Workbench};
use crate::platform::actions::ActionRegistry;
use crate::platform::i18n::Localization;
use crate::platform::settings;
use crate::platform::storage::UiStorage;
use crate::vcs::commit_box::{CommitBox, CommitBoxEvent, CommitBoxInput, CommitRequest};
use crate::vcs::commit_view::CommitPanel;
use crate::vcs::log_view::GitLog;
use crate::vcs_ops::change_ops::{commit_candidates, commit_precheck, commit_success, commit_title, committed};
use crate::vcs_ops::commit_selection::{PartialFiles, current_plan, selection_of};
use crate::vcs_ops::commit_store::{CommitState, history_key, parse_history, remembered};
use crate::vcs_ops::commit_ui::{CommitEntry, open_commit};
use gpui_kit::{App, AppContext, AsyncWindowContext, Context, Entity, Subscription, Window};
use ide_host::ops::vcs as git;
use ide_vcs::LogQuery;
use ide_workspace::settings::SettingsScope;
use serde_json::Value;
use std::path::PathBuf;

/// The commit state of the window's project, with the message box that edits it.
pub(super) struct CommitSlot {
    state: Entity<CommitState>,
    /// The box of the Commit tool window, once the window was shown.
    commit_box: Option<(Entity<CommitBox>, Subscription)>,
    /// A commit is running.
    busy: bool,
    _observe: Subscription,
}

pub(super) fn register_actions(registry: &mut ActionRegistry) {
    registry.handle("vcs.commit", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.open_commit(window, cx))
    });
    registry.handle("vcs.commitAndPush", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.commit_and_push(window, cx))
    });
    registry.handle("vcs.showMessageHistory", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| {
            workbench.show_commit_window(window, cx);
            let state = workbench.commit_state(cx);
            state.update(cx, |state, cx| {
                state.history_tick += 1;
                cx.notify();
            });
        })
    });
}

/// The settings key of a before-commit check.
fn check_setting(key: &str) -> String {
    format!("vcs.commit.{key}")
}

impl Workbench {
    /// The commit state of the window's project, made when first asked for.
    pub(super) fn commit_state(&mut self, cx: &mut Context<Self>) -> Entity<CommitState> {
        if let Some(slot) = &self.commit {
            return slot.state.clone();
        }
        let state = cx.new(|_| CommitState::default());
        let observe = cx.observe(&state, |workbench, _, cx| {
            workbench.sync_commit_box(cx);
            workbench.sync_commit_tabs(cx);
        });
        self.commit = Some(CommitSlot { state: state.clone(), commit_box: None, busy: false, _observe: observe });
        state
    }

    /// The project goes: its draft and its box go with it.
    pub(super) fn drop_commit(&mut self) {
        self.commit = None;
    }

    /// The message box of the Commit tool window, once the window was shown.
    pub fn commit_box(&self) -> Option<&Entity<CommitBox>> {
        self.commit.as_ref()?.commit_box.as_ref().map(|(commit_box, _)| commit_box)
    }

    /// Whether Amend is checked in the window's commit state.
    pub(super) fn commit_amend(&self, cx: &App) -> bool {
        self.commit.as_ref().is_some_and(|slot| slot.state.read(cx).amend)
    }

    /// Gives the changes tree of the Commit tool window its inclusion checkboxes and the message box.
    pub(super) fn attach_commit_box(
        &mut self,
        panel: &Entity<CommitPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.commit_state(cx);
        let input = self.commit_box_input(cx);
        let commit_box = cx.new(|cx| CommitBox::new(state.clone(), input, window, cx));
        let events = cx.subscribe_in(&commit_box, window, |workbench, _, event: &CommitBoxEvent, window, cx| {
            workbench.commit_box_event(event, window, cx);
        });
        panel.update(cx, |panel, cx| panel.set_commit(state, commit_box.clone(), cx));
        if let Some(slot) = self.commit.as_mut() {
            slot.commit_box = Some((commit_box, events));
        }
    }

    /// What the message box shows of the window: what is included, the merge, running work, the
    /// message history and the settings.
    fn commit_box_input(&self, cx: &Context<Self>) -> CommitBoxInput {
        let vcs = self.vcs_state();
        let included = match (vcs, self.commit.as_ref()) {
            (Some(vcs), Some(slot)) => current_plan(vcs, self.changelists(), slot.state.read(cx)).included.len(),
            _ => 0,
        };
        let operation = vcs.and_then(|vcs| vcs.branch.as_ref()?.operation.as_deref());
        let root = vcs.and_then(|vcs| vcs.root.as_deref());
        let stored = cx.try_global::<UiStorage>().and_then(|storage| storage.get(&history_key(root)));
        CommitBoxInput {
            included,
            merging: operation == Some("merge"),
            remote_busy: vcs.is_some_and(|vcs| vcs.remote_busy.is_some()),
            busy: self.commit.as_ref().is_some_and(|slot| slot.busy),
            progress: None,
            history: parse_history(stored),
            settings: settings::effective(&self.label, cx).vcs.commit,
            plugin_checks: Vec::new(),
            unavailable_checks: super::shelf::unavailable_commit_checks(),
        }
    }

    /// The message box shows the window's state again.
    pub(super) fn sync_commit_box(&mut self, cx: &mut Context<Self>) {
        let Some(commit_box) = self.commit_box().cloned() else { return };
        let input = self.commit_box_input(cx);
        commit_box.update(cx, |commit_box, cx| {
            if commit_box.input() != &input {
                commit_box.set_input(input, cx);
            }
        });
    }

    fn show_commit_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_layout(window, cx, |layout| layout.show(COMMIT_TOOL_WINDOW, true));
    }

    /// Commit…: shows the Commit tool window with the keyboard in the message field. The Commit
    /// dialog of the modal commit interface does not exist here: the window is shown instead.
    fn open_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The window comes first: a box made after the request would take it as already answered.
        self.show_commit_window(window, cx);
        let vcs = settings::effective(&self.label, cx).vcs;
        let state = self.commit_state(cx);
        let entry = state.update(cx, |state, cx| {
            let entry = open_commit(state, &vcs);
            cx.notify();
            entry
        });
        if entry == CommitEntry::Dialog {
            tracing::info!("the Commit dialog is not available: committing from the Commit tool window");
            state.update(cx, |state, cx| {
                state.focus_tick += 1;
                cx.notify();
            });
        }
    }

    /// Commit and Push…: with a message (or Amend) it commits, then shows the Push dialog; without
    /// one it shows where the message is typed.
    fn commit_and_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ready = self.commit.as_ref().is_some_and(|slot| {
            let state = slot.state.read(cx);
            !state.message.trim().is_empty() || state.amend
        });
        if !ready {
            self.open_commit(window, cx);
            return;
        }
        self.show_commit_window(window, cx);
        if let Some(commit_box) = self.commit_box().cloned() {
            commit_box.update(cx, |commit_box, cx| commit_box.commit(true, false, cx));
        }
    }

    fn commit_box_event(&mut self, event: &CommitBoxEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            CommitBoxEvent::Commit(request) => {
                if let Some(slot) = self.commit.as_mut() {
                    slot.busy = true;
                }
                let request = (**request).clone();
                cx.spawn_in(window, async move |this, cx| {
                    let ended = commit(&this, cx, request).await;
                    // The buttons can be pressed again, whatever came of it.
                    let _ = this.update(cx, |workbench, cx| {
                        if let Some(slot) = workbench.commit.as_mut() {
                            slot.busy = false;
                        }
                        workbench.sync_commit_box(cx);
                    });
                    finished(ended);
                })
                .detach();
            }
            CommitBoxEvent::LoadAmendMessage => {
                cx.spawn_in(window, async move |this, cx| finished(load_amend_message(&this, cx).await)).detach();
            }
            CommitBoxEvent::RememberMessage(message) => self.remember_commit_message(message, cx),
            CommitBoxEvent::SetCheck { check, on } => {
                self.write_commit_setting(&check_setting(check.key()), Value::Bool(*on), cx);
            }
            CommitBoxEvent::SetPluginCheck { key, on } => {
                let mut disabled = settings::effective(&self.label, cx).vcs.commit.disabled_plugin_checks;
                disabled.retain(|known| known != key);
                if !on {
                    disabled.push(key.clone());
                }
                let value = Value::Array(disabled.into_iter().map(Value::String).collect());
                self.write_commit_setting(&check_setting("disabledPluginChecks"), value, cx);
            }
            CommitBoxEvent::Configure => {
                run_action_later("file.settings".to_owned(), Some(Value::String("vcs.commit".to_owned())), window, cx);
            }
            CommitBoxEvent::ProblemDetails(index) => self.commit_problem_details(*index, window, cx),
        }
    }

    fn write_commit_setting(&mut self, key: &str, value: Value, cx: &mut Context<Self>) {
        if let Err(error) = settings::set(&self.label, SettingsScope::Global, key, &value, cx) {
            tracing::warn!("cannot store {key}: {}", error.message);
        }
        self.sync_commit_box(cx);
    }

    /// Puts `message` first in the project's commit message history, and stores it.
    pub(super) fn remember_commit_message(&mut self, message: &str, cx: &mut Context<Self>) {
        let key = history_key(self.vcs_state().and_then(|vcs| vcs.root.as_deref()));
        let history = parse_history(cx.try_global::<UiStorage>().and_then(|storage| storage.get(&key)));
        let Some(history) = remembered(&history, message) else { return };
        match serde_json::to_string(&history) {
            Ok(json) => cx.default_global::<UiStorage>().set(&key, json),
            Err(error) => tracing::warn!("cannot store the commit message history: {error}"),
        }
        self.sync_commit_box(cx);
    }

    /// The changes tree and the log read the repository again. The changes tree is fed by the
    /// window's read, which waits for a Git command of the window (a rebase after the commit).
    pub(super) fn reread_commit_views(&mut self, cx: &mut Context<Self>) {
        self.refresh_vcs(cx);
        if let Some(log) = self.tool_window_view(GIT_TOOL_WINDOW).and_then(|view| view.downcast::<GitLog>().ok()) {
            log.update(cx, |log, cx| log.reload(cx));
        }
    }
}

/// Commits what is included with the box's message and options; with `push`, the Push dialog
/// follows a commit that went through.
async fn commit(this: &This, cx: &mut AsyncWindowContext, request: CommitRequest) -> anyhow::Result<()> {
    save_all(this, cx).await?;
    refresh(this, cx).await?;
    let prepared = this.update(cx, |workbench, cx| {
        let state = workbench.commit_state(cx).read(cx).clone();
        let vcs = workbench.vcs_state().cloned().unwrap_or_default();
        let plan = current_plan(&vcs, workbench.changelists(), &state);
        let merging = vcs.branch.as_ref().and_then(|branch| branch.operation.as_deref()) == Some("merge");
        let clear = settings::effective(&workbench.label, cx).vcs.commit.clear_initial_message;
        (state, vcs, plan, merging, clear)
    })?;
    let (state, vcs, plan, merging, clear) = prepared;
    let amend = request.options.amend;
    if let Some(notice) = commit_precheck(&request.message, amend, &plan, merging) {
        return notify(this, cx, notice);
    }
    // Failed checks show in the box; the next Commit (Commit Anyway) skips them.
    if !request.anyway {
        let problems = super::shelf::run_commit_checks(this, cx, commit_candidates(&plan), amend).await?;
        if !problems.is_empty() {
            return this.update(cx, |workbench, cx| {
                let state = workbench.commit_state(cx);
                state.update(cx, |state, cx| {
                    state.check_problems = problems;
                    cx.notify();
                });
            });
        }
    }
    // Changed blocks are left out only from a diff, which has no checkboxes here: every included
    // file goes in whole.
    let mut partial = PartialFiles::default();
    for path in &plan.partial {
        partial.add(path, &[], &[], None);
    }
    let selection = selection_of(&plan, &state, &partial, &vcs, vcs.version);
    let message = request.message.trim().to_owned();
    let repositories = selection.repositories.len();
    let success = this.update(cx, |_, cx| {
        let i18n = &mut cx.default_global::<Localization>().0;
        commit_success(plan.included.len(), repositories, amend, &message, i18n)
    })?;
    let work = {
        let message = message.clone();
        move |host: std::sync::Arc<ide_host::Host>, label: String| async move {
            let mut id = String::new();
            for repo in selection.repositories {
                let root = Some(PathBuf::from(&repo.root.0)).filter(|root| !root.as_os_str().is_empty());
                id = git::vcs_commit(&host, &label, root, message.clone(), repo.options).await?;
            }
            Ok(id)
        }
    };
    let committed_id = run_op(this, cx, commit_title(amend), Some(success), false, work).await?;
    if committed_id.is_none() {
        return Ok(());
    }
    this.update(cx, |workbench, cx| {
        let root = workbench.vcs_state().and_then(|vcs| vcs.root.clone());
        let state = workbench.commit_state(cx);
        let done = state.update(cx, |state, cx| {
            let done = committed(state, &plan, &message, clear, root.as_deref());
            cx.notify();
            done
        });
        workbench.remember_commit_message(&done.remember, cx);
        workbench.reread_commit_views(cx);
    })?;
    if let Some(target) = request.rebase_onto.clone() {
        super::log_actions::rebase_after_fixup(this.clone(), cx, target).await?;
    }
    if request.push {
        push(this.clone(), cx, None).await?;
    }
    Ok(())
}

/// Amend was checked: the last commit's message replaces the draft.
async fn load_amend_message(this: &This, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let repo = vcs_state(this, cx, |vcs| vcs.repo.clone())?.map(PathBuf::from);
    let query = LogQuery { branch: Some("HEAD".to_owned()), limit: 1, ..LogQuery::default() };
    let last = call(this, cx, move |host, label| async move { git::vcs_log(&host, &label, repo, query).await }).await?;
    // A repository without commits (or one that cannot be read) has no message to amend.
    let message = match last {
        Ok(commits) => commits.into_iter().next().map(|commit| commit.message),
        Err(error) => {
            tracing::warn!("cannot read the last commit's message: {error}");
            None
        }
    };
    this.update(cx, |workbench, cx| {
        if let Some(commit_box) = workbench.commit_box().cloned() {
            commit_box.update(cx, |commit_box, cx| commit_box.amend_message_loaded(message.as_deref(), cx));
        }
    })
}
