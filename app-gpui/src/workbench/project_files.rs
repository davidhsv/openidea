//! The Project view's file operations in a project window: New, Rename, Move, Copy, Delete,
//! Copy Path and the tree's context menu, carried out on the backend.
//!
//! The operations are the models of [`crate::project_view`]: one [`ProjectViewState`] mirrors the
//! tree before every step, an operation under way is a [`Flow`] whose [`Step`]s the window carries
//! out (effects first, then the request, whose answer resumes the flow). One flow runs at a time; a
//! new one takes the place of one left waiting, and an answer for a flow that is gone is dropped.
//!
//! The New and Copy Path popups live in the popup slot. Rename, Move, Copy, Delete, the safe
//! delete questions and the refactoring preview are a dialog layer of their own below it, one at a
//! time. Backend work runs off the window's thread.

use super::services_window::on_workbench;
use super::{Content, PROJECT_TOOL_WINDOW, Workbench};
use crate::backend::Backend;
use crate::editor::view::{Editor, EditorClipboard};
use crate::editor_commands::copy_reference::{ReferenceInput, declared_name, qualified_reference, reference_offset};
use crate::editor_commands::outline::declaration_chain;
use crate::editor_refactor::safe_delete::{FileUsagesAction, FileUsagesFlow};
use crate::platform::actions::{ActionRegistry, ActionSource, execute_action_from, is_action_enabled};
use crate::platform::context::window_context;
use crate::platform::i18n::Localization;
use crate::platform::input::Keymap;
use crate::platform::local_time::now_ms;
use crate::platform::misc::copy_path::PathKind;
use crate::platform::misc::platform_info::Os;
use crate::platform::project_roots::OpenProject;
use crate::platform::storage::UiStorage;
use crate::project_tree::{ContextMenuRequested, ProjectTree};
use crate::project_tree_model::ProjectNode;
use crate::project_tree_paths::{is_same_or_inside, parent_of};
use crate::project_view::actions::{folder_from, nodes_from};
use crate::project_view::context_menu::{
    Contributed, MenuContext, NewKind, ProjectCommand, context_menu_items, new_menu_items, run_command, shown_items,
};
use crate::project_view::copy_path_popup::{CopyPathEvent, CopyPathPopup, REFERENCE, copied_hint, copy_path_items};
use crate::project_view::file_dialogs::{
    COPY_OPEN_KEY, DeleteDialog, DeleteDialogEvent, FileDialog, FileDialogEvent, FileDialogInput, PreviewInput,
    RefactorOptions, RefactoringPreview, RefactoringPreviewEvent, delete_reply, load_refactor_options, progress_reply,
    refactor_options_key, usages_detected_request, usages_reply,
};
use crate::project_view::file_refactor::{
    describe_moves, done_notification, errors_notification, failed_notification, label_text, merge_edits, moved_path,
    touches_other_files, undo_failed_notification, undone_notification,
};
use crate::project_view::fs::{BackendCall, FsRequest, backend_call};
use crate::project_view::new_popup::{NameRule, NewPopupEvent, NewPopupView, PopupActionSet};
use crate::project_view::new_popup_model::{NewPopupRequest, PopupRun};
use crate::project_view::operations::{
    Effect, Env, FileOperations, FileRefactorRequest, Flow, Next, Outcome, RefactorLabel, RefactorStatus, Reply,
    Request, SAFE_DELETE_KEY, Step, UsageSearch, copy_dialog_confirmed, copy_paths, delete_selection, lead_node,
    move_dialog_done, node_refs, open_copy_dialog, open_move_dialog, open_rename_dialog, paths_for, rename_dialog_done,
    selected_nodes, selection_target_folder,
};
use crate::project_view::store::{ProjectViewState, expanded_key};
use crate::vcs::prompt_dialogs::{DialogAnswer, PromptDialog, PromptDialogEvent};
use crate::vcs_ops::dialogs::AskRequest;
use crate::workbench::notifications::{NotificationLevel, NotificationRequest};
use futures::channel::oneshot;
use gpui_kit::{
    AnyElement, AnyView, App, AsyncWindowContext, BorrowAppContext, ClipboardEntry, ClipboardItem, Context, Entity,
    FocusHandle, Focusable, IntoElement, PathPromptOptions, Subscription, Task, WeakEntity, Window, div, prelude::*,
};
use ide_core::error::ErrorKind;
use ide_core::{IdeError, IdeEvent, IdeResult, OutlineItem, Pos, RequestId, SymbolKind, WorkspaceEdit};
use ide_host::Host;
use ide_workspace::FileMove;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What is carried out once a flow is done.
pub(super) type Done = Box<dyn FnOnce(&mut Workbench, Outcome, &mut Window, &mut Context<Workbench>)>;

/// The operation under way.
struct Running {
    id: u64,
    flow: Flow,
    /// Invoked from the tree or its menu: the tree gets the keyboard back afterwards.
    in_tree: bool,
    done: Option<Done>,
}

/// What the dialog layer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogKind {
    /// Rename, Move or Copy.
    File,
    Delete,
    /// "Looking for usages…" of a safe delete.
    Progress,
    Question,
    Preview,
}

struct DialogSlot {
    view: AnyView,
    kind: DialogKind,
    /// What had the keyboard before the dialog opened.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

/// Stops a search on the backend: a usage search, or a refactoring's reference search.
#[derive(Default)]
struct SearchStop {
    stopped: AtomicBool,
    /// The cancellable requests under way.
    requests: Mutex<Vec<RequestId>>,
}

impl SearchStop {
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn track(&self, request: RequestId) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request);
        }
    }

    fn untrack(&self, request: RequestId) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.retain(|tracked| *tracked != request);
        }
    }

    /// Marks the search stopped and cancels what it has under way.
    fn stop(&self, host: &Host) {
        self.stopped.store(true, Ordering::SeqCst);
        let requests = self.requests.lock().map(|requests| requests.clone()).unwrap_or_default();
        for request in requests {
            host.cancel_request(request);
        }
    }
}

/// The Project view's file operations of a project window.
#[derive(Default)]
pub(super) struct ProjectFiles {
    state: ProjectViewState,
    log: FileOperations,
    flow: Option<Running>,
    next_flow: u64,
    /// Wakes the flow when its next deadline falls due.
    deadline: Option<Task<()>>,
    dialog: Option<DialogSlot>,
    /// The backend's id of the last file refactoring, which Undo reverts.
    last_move: Option<u32>,
    /// The usage search of a safe delete under way.
    usage_search: Option<Arc<SearchStop>>,
    /// The reference search of a Rename or Move dialog under way.
    reference_search: Option<Arc<SearchStop>>,
    /// The New popup that is open.
    new_popup: Option<WeakEntity<NewPopupView>>,
}

/// The actions the window's Project view carries out.
pub(super) fn register_actions(registry: &mut ActionRegistry) {
    registry.handle("project.newFile", |call, window, cx| {
        let (focus, editor) = (call.ctx.is_truthy("projectViewFocus"), call.ctx.is_truthy("editorFocus"));
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            // In the Project view the New popup is for the selection's folder; in an editor, for
            // the edited file's folder. Otherwise a new file in the folder given, or the selection's.
            if arg.is_none() && focus && workbench.show_new_popup(None, false, None, window, cx) {
                return;
            }
            if arg.is_none() && editor && workbench.show_new_popup_for_editor(false, window, cx) {
                return;
            }
            workbench.new_item(NewKind::File, arg.as_ref(), window, cx);
        })
    });
    registry.handle("project.newInThisDirectory", |call, window, cx| {
        let focus = call.ctx.is_truthy("projectViewFocus");
        on_workbench(window, cx, move |workbench, window, cx| {
            if !(focus && workbench.show_new_popup(None, false, None, window, cx)) {
                workbench.show_new_popup_for_editor(true, window, cx);
            }
        })
    });
    registry.handle("file.new", |call, window, cx| {
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            workbench.new_item(NewKind::File, arg.as_ref(), window, cx)
        })
    });
    registry.handle("project.newFolder", |call, window, cx| {
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            workbench.new_item(NewKind::Directory, arg.as_ref(), window, cx);
        })
    });
    registry.handle("project.rename", |call, window, cx| {
        let focus = call.ctx.is_truthy("projectViewFocus");
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            // From the editor (tabs, Find Action) the active file; else the selection.
            let arg = arg.or_else(|| (!focus).then(|| workbench.active_file_path(cx).map(Value::String)).flatten());
            let nodes = workbench.nodes_for(arg.as_ref(), cx);
            let node = match arg {
                Some(_) => nodes.first().cloned(),
                None => lead_node(&workbench.files.state),
            };
            workbench.start_flow(open_rename_dialog(node.as_deref()), focus, None, window, cx);
        })
    });
    registry.handle("project.move", |call, window, cx| {
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            let nodes = workbench.nodes_for(arg.as_ref(), cx);
            let effects = open_move_dialog(&workbench.files.state, &node_refs(&nodes), None);
            workbench.start_flow(crate::project_view::operations::immediate(effects), true, None, window, cx);
        })
    });
    registry.handle("project.copyElement", |call, window, cx| {
        let focus = call.ctx.is_truthy("projectViewFocus");
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            let arg = arg.or_else(|| (!focus).then(|| workbench.active_file_path(cx).map(Value::String)).flatten());
            let nodes = workbench.nodes_for(arg.as_ref(), cx);
            let effects = open_copy_dialog(&workbench.files.state, &node_refs(&nodes));
            workbench.start_flow(crate::project_view::operations::immediate(effects), focus, None, window, cx);
        })
    });
    registry.handle("project.delete", |call, window, cx| {
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            let nodes = workbench.nodes_for(arg.as_ref(), cx);
            let started =
                workbench.with_view(true, window, cx, |state, env| delete_selection(state, env, &node_refs(&nodes)));
            workbench.start_flow(started, true, None, window, cx);
        })
    });
    registry.handle("project.copyAbsolutePath", |call, window, cx| {
        let arg = call.arg.cloned();
        on_workbench(window, cx, move |workbench, window, cx| {
            let nodes = workbench.nodes_for(arg.as_ref(), cx);
            let root = workbench.files.state.root.clone().unwrap_or_default();
            let name = workbench.files.state.name.clone();
            let project = Some(OpenProject { root: &root, name: &name, workspace: None });
            let paths = paths_for(&workbench.files.state, PathKind::Absolute, &node_refs(&nodes), project, &[]);
            workbench.start_flow(copy_paths(&paths), true, None, window, cx);
        })
    });
    // Cut / Copy / Paste of files while the tree has the keyboard; in the editor the same keys stay
    // the editor's (`edit.*`), as the keymap binds these to `projectViewFocus`.
    for (id, command) in [("project.cut", ProjectCommand::Cut), ("project.copy", ProjectCommand::Copy)] {
        registry.handle(id, move |_, window, cx| {
            let command = command.clone();
            on_workbench(window, cx, move |workbench, window, cx| {
                workbench.run_project_command(command, Vec::new(), true, window, cx);
            })
        });
    }
    registry.handle("project.paste", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| {
            let target = workbench.with_view(true, window, cx, |state, _| selection_target_folder(state));
            workbench.run_project_command(ProjectCommand::Paste { target }, Vec::new(), true, window, cx);
        })
    });
    registry.handle("project.undoMove", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.undo_move(window, cx))
    });
    registry.handle("editor.copyPathReference", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.show_copy_path_popup(window, cx))
    });
    registry.handle("editor.copyReference", |call, window, cx| {
        let given = call.arg.and_then(Value::as_str).map(str::to_owned);
        on_workbench(window, cx, |workbench, window, cx| workbench.copy_reference(given, window, cx))
    });
}

/// The OS the window and the project's files are on.
fn this_os() -> Os {
    Os::from_platform(std::env::consts::OS)
}

/// The editor tabs' input kind of a file.
const FILE_TAB: &str = "file";

/// The reference search's requests: what the language servers and the text search found.
struct Collected {
    edit: WorkspaceEdit,
    /// Language servers that could not answer.
    errors: Vec<String>,
}

/// Runs `work` on the backend with the host and the window's label.
fn host_call<T, F, W>(label: String, cx: &App, work: W) -> impl Future<Output = IdeResult<T>> + 'static + use<T, F, W>
where
    T: Send + 'static,
    F: Future<Output = IdeResult<T>> + Send + 'static,
    W: FnOnce(Arc<Host>, String) -> F + Send + 'static,
{
    let call = cx.try_global::<Backend>().map(|backend| backend.with_host(move |host| work(host, label)));
    async move {
        match call {
            None => Err(IdeError::unavailable("The backend is not running")),
            Some(call) => call.await.unwrap_or_else(|error| Err(IdeError::internal(format!("{error:#}")))),
        }
    }
}

/// What Copy Reference names at `pos`: the symbols down to the one whose name is there, packages
/// left out, each by its bare name; empty off every name.
fn symbol_chain(items: &[OutlineItem], pos: Pos) -> Vec<String> {
    let chain = declaration_chain(items, pos).into_iter().filter(|item| item.kind != SymbolKind::Package);
    chain.map(|item| declared_name(&item.name, item.kind).to_owned()).collect()
}

/// The usages of the symbols `files` declare, from outside them, as the language servers find
/// them; stopped through `stop`.
async fn file_usages(host: Arc<Host>, label: String, files: Vec<String>, stop: Arc<SearchStop>) -> UsageSearch {
    let (mut flow, actions) = FileUsagesFlow::start(files);
    let mut queue: VecDeque<FileUsagesAction> = actions.into();
    while let Some(action) = queue.pop_front() {
        let more = match action {
            FileUsagesAction::Done(found) => return found,
            FileUsagesAction::CloseDocument(doc) => {
                if let Err(error) = ide_host::ops::doc::close(&host, &label, doc) {
                    tracing::debug!("safe delete: cannot close a document: {}", error.message);
                }
                Vec::new()
            }
            _ if stop.is_stopped() => flow.cancel(),
            FileUsagesAction::Document { ticket, path } => {
                let opened = ide_host::ops::doc::open(&host, &label, PathBuf::from(&path)).await;
                flow.document(ticket, opened.map(|info| (info.id, true)))
            }
            FileUsagesAction::Outline { ticket, doc } => {
                flow.outline(ticket, ide_host::ops::doc::doc_outline(&host, &label, doc).await)
            }
            FileUsagesAction::References { ticket, doc, pos } => {
                let request = Backend::next_request_id();
                stop.track(request);
                let channel = ide_host::ops::Channel::new(|_| {});
                let found =
                    ide_host::ops::lsp::lsp_references(&host, &label, request, doc, pos, false, false, false, channel)
                        .await;
                stop.untrack(request);
                flow.references(ticket, found.map(|references| references.locations))
            }
        };
        queue.extend(more);
    }
    UsageSearch::Failed("The usage search ended without an answer".to_owned())
}

/// Collects the reference updates of `moves`: the language servers' with `references`, the
/// textual occurrences with `text_occurrences`. `None` when the search was stopped.
async fn collect_edits(
    this: &WeakEntity<Workbench>,
    cx: &mut AsyncWindowContext,
    moves: &[FileMove],
    options: RefactorOptions,
    stop: &Arc<SearchStop>,
) -> anyhow::Result<Option<Collected>> {
    let mut collected = Collected { edit: WorkspaceEdit::default(), errors: Vec::new() };
    let mut lsp = WorkspaceEdit::default();
    let mut text = WorkspaceEdit::default();
    if options.references {
        let (request, sent, tracker) = (Backend::next_request_id(), moves.to_vec(), stop.clone());
        tracker.track(request);
        let call = this.update(cx, |workbench, cx| {
            host_call(workbench.label.clone(), cx, move |host, label| async move {
                ide_host::ops::refactor::refactor_file_move_edits(&host, &label, request, sent).await
            })
        })?;
        let found = call.await;
        stop.untrack(request);
        match found {
            Ok(found) => {
                lsp = found.edit;
                collected.errors.extend(found.errors);
            }
            Err(error) if error.kind == ErrorKind::Cancelled => return Ok(None),
            Err(error) => collected.errors.push(error.message),
        }
    }
    if stop.is_stopped() {
        return Ok(None);
    }
    if options.text_occurrences {
        let (request, sent) = (Backend::next_request_id(), moves.to_vec());
        stop.track(request);
        let call = this.update(cx, |workbench, cx| {
            host_call(workbench.label.clone(), cx, move |host, label| async move {
                ide_host::ops::refactor::refactor_file_move_text_edits(&host, &label, request, sent).await
            })
        })?;
        let found = call.await;
        stop.untrack(request);
        match found {
            Ok(found) => text = found,
            Err(error) if error.kind == ErrorKind::Cancelled => return Ok(None),
            Err(error) => collected.errors.push(error.message),
        }
    }
    if stop.is_stopped() {
        return Ok(None);
    }
    collected.edit = merge_edits(&lsp, &text);
    Ok(Some(collected))
}

/// Saves every unsaved editor: the servers and the text search read the files from disk, and the
/// move takes the files as they are there. A failure is reported by the save itself.
async fn save_all(this: &WeakEntity<Workbench>, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let saving = this.update(cx, |workbench, cx| workbench.save_all(cx))?;
    if let Err(error) = saving.await {
        this.update(cx, |workbench, cx| workbench.report_save_failure(error, cx))?;
    }
    Ok(())
}

/// Moves `moves` as the refactoring `label`: the reference updates, a preview when they touch other
/// files, the move itself (one step Undo reverts), and what the user is told. `dialog` is the
/// dialog it was started from, closed once the references are collected.
async fn run_refactoring(
    this: WeakEntity<Workbench>,
    cx: &mut AsyncWindowContext,
    moves: Vec<FileMove>,
    options: RefactorOptions,
    label: RefactorLabel,
    dialog: Option<AnyView>,
    stop: Arc<SearchStop>,
) -> anyhow::Result<RefactorStatus> {
    if moves.is_empty() {
        return Ok(RefactorStatus::Cancelled);
    }
    let searching = options.references || options.text_occurrences;
    if searching {
        save_all(&this, cx).await?;
    }
    let collected = if stop.is_stopped() {
        None
    } else if searching {
        collect_edits(&this, cx, &moves, options, &stop).await?
    } else {
        Some(Collected { edit: WorkspaceEdit::default(), errors: Vec::new() })
    };
    this.update_in(cx, |workbench, window, cx| {
        workbench.files.reference_search = None;
        if let Some(dialog) = &dialog {
            workbench.close_dialog_of(dialog, window, cx);
        }
    })?;
    let Some(collected) = collected else { return Ok(RefactorStatus::Cancelled) };
    let title = describe_moves(&moves, label);
    let mut edit = collected.edit;
    if touches_other_files(&edit, &moves) {
        let (input_title, shown) = (title.clone(), edit.clone());
        let answer =
            this.update_in(cx, |workbench, window, cx| workbench.show_preview(input_title, shown, window, cx))?;
        match answer.await {
            Ok(Some(kept)) => edit = kept,
            _ => return Ok(RefactorStatus::Cancelled),
        }
    }
    // Editors follow the files: what they hold is saved first.
    save_all(&this, cx).await?;
    let (sent, sent_edit, name) = (moves.clone(), edit.clone(), label_text(label).to_owned());
    let call = this.update(cx, |workbench, cx| {
        host_call(workbench.label.clone(), cx, move |host, window| async move {
            ide_host::ops::refactor::refactor_move_files(&host, &window, sent, sent_edit, name).await
        })
    })?;
    let moved = call.await;
    this.update_in(cx, |workbench, window, cx| match moved {
        Err(error) => {
            workbench.notify(failed_notification(&moves, label, &error.message), cx);
            RefactorStatus::Failed
        }
        Ok(id) => {
            workbench.files.last_move = Some(id);
            workbench.files.log.record_move();
            workbench.files_moved(&moves, window, cx);
            if let Some(warning) = errors_notification(&collected.errors) {
                workbench.notify(warning, cx);
            }
            let done = done_notification(&mut cx.default_global::<Localization>().0, &title, edit.files.len());
            workbench.notify(done, cx);
            RefactorStatus::Done
        }
    })
}

impl Workbench {
    // ---------------------------------------------------------------------------------------------
    // The state and the flows

    /// Runs `run` on the Project view's state as the tree shows it now, with the surroundings the
    /// operations know; what it changed in the expansion and the row to reveal goes back to the tree.
    pub(super) fn with_view<R>(
        &mut self,
        in_tree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        run: impl FnOnce(&mut ProjectViewState, &mut Env<'_>) -> R,
    ) -> R {
        let tree = self.project_tree().cloned();
        if let Some(tree) = &tree {
            tree.read(cx).mirror_into(&mut self.files.state);
        }
        let storage = cx.try_global::<UiStorage>();
        let stored = |key: &str| storage.and_then(|storage| storage.get(key)).map(str::to_owned);
        let stored_expanded = self.files.state.root.as_deref().and_then(|root| stored(&expanded_key(root)));
        let stored_safe_delete = stored(SAFE_DELETE_KEY);
        let moved = stored(&refactor_options_key(RefactorLabel::Move));
        let open_moved_in_editor = load_refactor_options(moved.as_deref()).open_in_editor;
        cx.default_global::<Localization>();
        let files = &mut self.files;
        let result = cx.update_global::<Localization, _>(|localization, _| {
            let mut env = Env {
                i18n: &mut localization.0,
                log: &mut files.log,
                host_os: this_os(),
                os: this_os(),
                remote: false,
                now_ms: now_ms(),
                stored_expanded: stored_expanded.as_deref(),
                stored_safe_delete: stored_safe_delete.as_deref(),
                open_moved_in_editor,
                in_tree,
            };
            run(&mut files.state, &mut env)
        });
        if let Some(tree) = tree {
            let focus = tree.update(cx, |tree, cx| tree.follow_state(&mut self.files.state, cx));
            if focus == Some(true) {
                let handle = tree.focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        result
    }

    /// Starts the reference search of a refactoring: from now on its dialog's Cancel stops it, also
    /// before the search itself has begun.
    fn start_reference_search(&mut self) -> Arc<SearchStop> {
        let stop = Arc::new(SearchStop::default());
        self.files.reference_search = Some(stop.clone());
        stop
    }

    /// Starts an operation in place of any that waits; `done` runs once it is done.
    pub(super) fn start_flow(
        &mut self,
        (flow, step): (Flow, Step),
        in_tree: bool,
        done: Option<Done>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.files.next_flow += 1;
        let id = self.files.next_flow;
        self.files.flow = Some(Running { id, flow, in_tree, done });
        self.carry_out(id, step, window, cx);
    }

    /// Continues the flow `id` with the answer to its request; dropped when that flow is gone.
    fn resume_flow(&mut self, id: u64, reply: Reply, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut running) = self.files.flow.take_if(|running| running.id == id) else { return };
        let step = self.with_view(running.in_tree, window, cx, |state, env| running.flow.resume(state, env, reply));
        if self.files.flow.is_none() {
            self.files.flow = Some(running);
        }
        self.carry_out(id, step, window, cx);
    }

    fn carry_out(&mut self, id: u64, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        for effect in step.effects {
            self.carry_out_effect(id, effect, window, cx);
        }
        match step.next {
            Next::Wait(request) => self.answer(id, request, window, cx),
            Next::Waiting => {}
            Next::Done(outcome) => {
                if let Some(running) = self.files.flow.take_if(|running| running.id == id)
                    && let Some(done) = running.done
                {
                    done(self, outcome, window, cx);
                }
            }
        }
        self.schedule_flow_deadline(id, window, cx);
    }

    fn schedule_flow_deadline(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.files.flow.as_ref().filter(|running| running.id == id);
        let Some(due) = running.and_then(|running| running.flow.next_deadline()) else {
            if running.is_some() || self.files.flow.is_none() {
                self.files.deadline = None;
            }
            return;
        };
        let wait = Duration::from_secs_f64(((due - now_ms()) / 1000.0).max(0.0));
        self.files.deadline = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update_in(cx, |workbench, window, cx| workbench.expire_flow(id, window, cx));
        }));
    }

    /// Carries out what fell due in the flow `id`: the usage search's progress, or its stop.
    fn expire_flow(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(running) = self.files.flow.as_mut().filter(|running| running.id == id) else { return };
        for effect in running.flow.expire(now_ms()) {
            self.carry_out_effect(id, effect, window, cx);
        }
        self.schedule_flow_deadline(id, window, cx);
    }

    /// The nodes an action acts on: its argument's paths, else the tree's selection.
    pub(super) fn nodes_for(&mut self, arg: Option<&Value>, cx: &mut Context<Self>) -> Vec<Arc<ProjectNode>> {
        if let Some(tree) = self.project_tree().cloned() {
            tree.read(cx).mirror_into(&mut self.files.state);
        }
        nodes_from(&self.files.state, arg)
    }

    pub(super) fn active_file_path(&self, cx: &App) -> Option<String> {
        self.editor().map(|editor| editor.read(cx).path().to_string_lossy().into_owned())
    }

    // ---------------------------------------------------------------------------------------------
    // Effects

    fn carry_out_effect(&mut self, id: u64, effect: Effect, window: &mut Window, cx: &mut Context<Self>) {
        match effect {
            Effect::Notify(request) => self.notify(request, cx),
            Effect::OpenFile { path, .. } => self.open_project_file(PathBuf::from(path), window, cx),
            Effect::LoadDir { path, force } => {
                if let Some(tree) = self.project_tree().cloned() {
                    tree.update(cx, |tree, cx| tree.load_dir(&path, force, cx));
                }
            }
            Effect::RunAction { id: action, arg } => self.run_flow_action(action, arg, window, cx),
            Effect::Store(write) => {
                if cx.has_global::<UiStorage>() {
                    cx.global_mut::<UiStorage>().set(&write.key, write.value);
                }
            }
            Effect::OpenDialog(request) => self.show_file_dialog(request, window, cx),
            Effect::RevealInFileManager(paths) => super::project_view_actions::reveal_in_file_manager(paths, cx),
            Effect::ViewUsages { name, usages } => {
                let arg = serde_json::json!({ "symbol": name, "locations": usages });
                self.run_flow_action("find.showUsageResults", arg, window, cx);
            }
            Effect::HandOff { clipboard, write } => {
                // The OS takes text from here; a file list is kept on the internal clipboard only.
                let written = match write {
                    crate::project_view::clipboard::OsClipboardWrite::Text(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        true
                    }
                    crate::project_view::clipboard::OsClipboardWrite::Files(_) => false,
                };
                self.files.state.os_write_done(clipboard, written);
            }
            // Uploads only happen in a remote window: a local import is a copy.
            Effect::BeginLocalImport | Effect::FinishLocalImport => {}
            Effect::ShowSearchProgress(ask) => {
                self.ask_for_flow(id, ask, DialogKind::Progress, progress_reply, window, cx)
            }
            Effect::HideSearchProgress => {
                if self.files.dialog.as_ref().is_some_and(|dialog| dialog.kind == DialogKind::Progress) {
                    self.close_file_dialog(window, cx);
                }
            }
            Effect::StopUsageSearch => {
                if let (Some(stop), Some(backend)) = (self.files.usage_search.clone(), cx.try_global::<Backend>()) {
                    stop.stop(&backend.host);
                }
            }
            Effect::ShowProjectView { focus } => {
                self.with_layout(window, cx, |layout| layout.show(PROJECT_TOOL_WINDOW, focus));
            }
            Effect::MarkDirectory { path, role } => self.mark_directory_as(path, role, window, cx),
        }
    }

    /// Runs another feature's action for a flow. The editor's hint has no view here: it shows as
    /// a notification.
    fn run_flow_action(&mut self, action: &'static str, arg: Value, window: &mut Window, cx: &mut Context<Self>) {
        let registered = cx.try_global::<ActionRegistry>().is_some_and(|registry| registry.contains(action));
        if registered {
            window.defer(cx, move |window, cx| {
                execute_action_from(ActionSource::Other, action, Some(&arg), window, cx);
            });
        } else if action == "editor.showHint" {
            let text = arg.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let request = NotificationRequest {
                key: Some("project.copyPath".to_owned()),
                ..NotificationRequest::new(NotificationLevel::Info, text)
            };
            self.notify(request, cx);
        } else {
            let request = NotificationRequest {
                body: Some(action.to_owned()),
                ..NotificationRequest::new(NotificationLevel::Warning, "This action is not available here")
            };
            self.notify(request, cx);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Requests

    fn answer(&mut self, id: u64, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            Request::LoadDirs(dirs) => {
                let tree = self.project_tree().cloned();
                let mut wanted: Vec<String> = Vec::new();
                for dir in dirs {
                    let loaded = tree.as_ref().is_some_and(|tree| tree.read(cx).is_loaded(&dir.path));
                    if (dir.force || !loaded) && !wanted.contains(&dir.path) {
                        wanted.push(dir.path);
                    }
                }
                if wanted.is_empty() || tree.is_none() {
                    return self.resume_flow(id, Reply::Loaded, window, cx);
                }
                let call = host_call(self.label.clone(), cx, move |host, label| async move {
                    let mut reads = Vec::new();
                    for path in wanted {
                        let read = ide_host::ops::fs::read_dir(&host, &label, PathBuf::from(&path)).await;
                        reads.push((path, read));
                    }
                    Ok(reads)
                });
                self.on_answer(call, window, cx, move |workbench, reads, window, cx| {
                    if let (Some(tree), Ok(reads)) = (workbench.project_tree().cloned(), reads) {
                        tree.update(cx, |tree, cx| {
                            for (path, read) in reads {
                                tree.put_read(&path, read, cx);
                            }
                        });
                    }
                    workbench.resume_flow(id, Reply::Loaded, window, cx);
                });
            }
            Request::Fs(request) => {
                let call = backend_call(&request);
                let work = host_call(self.label.clone(), cx, move |host, label| async move {
                    match call {
                        BackendCall::FsReadDir(path) => {
                            ide_host::ops::fs::read_dir(&host, &label, PathBuf::from(path)).await.map(|_| ())
                        }
                        BackendCall::FsCreateFile(path) => ide_host::ops::fs::create_file(&host, &label, path.into()),
                        // What the desktop app's `fs_create_dir` command does.
                        BackendCall::FsCreateDir(path) => ide_vfs::create_dir(Path::new(&path)),
                        BackendCall::FsRename { from, to } => {
                            ide_host::ops::fs::rename(&host, from.into(), to.into()).await
                        }
                        BackendCall::FsDelete(path) => crate::backend::delete_path(&host, &label, path.into()).await,
                        BackendCall::FsCopy { from, to } => {
                            ide_host::ops::fs::copy(&host, from.into(), to.into()).await
                        }
                        BackendCall::RemoteImportLocal { .. } => {
                            Err(IdeError::unavailable("Uploading files needs a remote project"))
                        }
                    }
                });
                self.on_answer(work, window, cx, move |workbench, result, window, cx| {
                    if let (Ok(()), FsRequest::Delete(path)) = (&result, &request) {
                        workbench.close_removed_tabs(std::slice::from_ref(path), window, cx);
                    }
                    workbench.resume_flow(id, Reply::Fs(result.map(|()| Vec::new())), window, cx);
                });
            }
            Request::ConfirmDelete(question) => {
                let dialog = cx.new(|cx| DeleteDialog::new(question, cx));
                let events =
                    cx.subscribe_in(&dialog, window, move |workbench, _, event: &DeleteDialogEvent, window, cx| {
                        let DeleteDialogEvent::Answered(answer) = event;
                        workbench.close_file_dialog(window, cx);
                        workbench.resume_flow(id, delete_reply(*answer), window, cx);
                    });
                let focus = dialog.focus_handle(cx);
                self.open_file_dialog(dialog.into(), DialogKind::Delete, focus, events, window, cx);
            }
            Request::FileUsages(files) => {
                let stop = Arc::new(SearchStop::default());
                self.files.usage_search = Some(stop.clone());
                let search = host_call(self.label.clone(), cx, move |host, label| async move {
                    Ok(file_usages(host, label, files, stop).await)
                });
                self.on_answer(search, window, cx, move |workbench, found, window, cx| {
                    workbench.files.usage_search = None;
                    let found = found.unwrap_or_else(|error| UsageSearch::Failed(error.message));
                    workbench.resume_flow(id, Reply::Usages(found), window, cx);
                });
            }
            Request::UsagesDetected { subject, usages, plural } => {
                let i18n = &mut cx.default_global::<Localization>().0;
                let ask = usages_detected_request(i18n, &subject, &usages, plural);
                self.ask_for_flow(id, ask, DialogKind::Question, usages_reply, window, cx);
            }
            Request::Refactor { moves, label } => {
                let stored = cx.try_global::<UiStorage>().and_then(|storage| storage.get(&refactor_options_key(label)));
                let options = load_refactor_options(stored);
                let stop = self.start_reference_search();
                cx.spawn_in(window, async move |this, cx| {
                    let status = run_refactoring(this.clone(), cx, moves, options, label, None, stop).await;
                    let status = status.unwrap_or(RefactorStatus::Failed);
                    let _ = this.update_in(cx, |workbench, window, cx| {
                        workbench.resume_flow(id, Reply::Refactored(status), window, cx);
                    });
                })
                .detach();
            }
            Request::HistoryListDir(path) => {
                let call = host_call(self.label.clone(), cx, move |host, label| async move {
                    ide_host::ops::history::history_list_dir(&host, &label, PathBuf::from(path)).await
                });
                self.on_answer(call, window, cx, move |workbench, history, window, cx| {
                    workbench.resume_flow(id, Reply::History(history), window, cx);
                });
            }
            Request::HistoryRestore { path, id: snapshot } => {
                let call = host_call(self.label.clone(), cx, move |host, label| async move {
                    ide_host::ops::history::history_restore(&host, &label, PathBuf::from(path), snapshot).await
                });
                self.on_answer(call, window, cx, move |workbench, restored, window, cx| {
                    workbench.resume_flow(id, Reply::Restored(restored), window, cx);
                });
            }
            Request::OsClipboardFiles => {
                let files: Vec<String> = cx
                    .read_from_clipboard()
                    .map(|item| {
                        let entries = item.entries().iter();
                        let paths = entries.filter_map(|entry| match entry {
                            ClipboardEntry::ExternalPaths(paths) => Some(paths.paths().to_vec()),
                            _ => None,
                        });
                        paths.flatten().map(|path| path.to_string_lossy().into_owned()).collect()
                    })
                    .unwrap_or_default();
                self.resume_flow(id, Reply::OsClipboardFiles(Ok(files)), window, cx);
            }
            Request::WriteText(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.resume_flow(id, Reply::TextWritten(Ok(())), window, cx);
            }
        }
    }

    /// Awaits `work` and hands its result to `then` on the window's thread.
    fn on_answer<T: 'static>(
        &mut self,
        work: impl Future<Output = IdeResult<T>> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, IdeResult<T>, &mut Window, &mut Context<Self>) + 'static,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |workbench, window, cx| then(workbench, result, window, cx));
        })
        .detach();
    }

    // ---------------------------------------------------------------------------------------------
    // Editors

    /// Closes the tabs of files that are gone (`removed` and what is inside them); a tab with
    /// unsaved changes stays, since a deletion never discards edits.
    fn close_removed_tabs(&mut self, removed: &[String], window: &mut Window, cx: &mut Context<Self>) {
        let Content::Project { tabs, editors, .. } = &mut self.content else { return };
        let dirty = |id: &str| editors.get(id).is_some_and(|editor: &Entity<Editor>| editor.read(cx).is_dirty());
        let mut gone: Vec<String> = Vec::new();
        for pane in &tabs.panes {
            for tab in &pane.tabs {
                let Some(path) = tab.input.path.as_deref() else { continue };
                if removed.iter().any(|at| is_same_or_inside(path, at)) && !dirty(&tab.id) {
                    gone.push(tab.input.key.clone());
                }
            }
        }
        let mut changed = false;
        for key in gone {
            changed |= tabs.close_key(&key);
        }
        if changed {
            self.tabs_changed(false, window, cx);
        }
    }

    /// The files of `moves` are where the moves put them: the tree and version control hear of it,
    /// and the tabs of the moved files show them at their new paths.
    fn files_moved(&mut self, moves: &[FileMove], window: &mut Window, cx: &mut Context<Self>) {
        if let Some(backend) = cx.try_global::<Backend>() {
            let created = moves.iter().map(|moved| moved.to.clone()).collect();
            let removed = moves.iter().map(|moved| moved.from.clone()).collect();
            backend.emit(Some(&self.label), IdeEvent::FsChanged { created, modified: Vec::new(), removed });
        }
        let Content::Project { tabs, editors, editor_focus, find_bars, view_tabs, .. } = &mut self.content else {
            return;
        };
        let mut changed = false;
        for pane in &mut tabs.panes {
            for tab in &mut pane.tabs {
                let Some(path) = tab.input.path.clone().filter(|_| tab.input.kind == FILE_TAB) else { continue };
                let to = moved_path(&path, moves);
                // An editor whose text could not be saved keeps it, at the old path.
                if to == path || editors.get(&tab.id).is_some_and(|editor| editor.read(cx).is_dirty()) {
                    continue;
                }
                tab.input.key = to.clone();
                tab.input.path = Some(to);
                editors.remove(&tab.id);
                editor_focus.remove(&tab.id);
                find_bars.remove(&tab.id);
                view_tabs.drop_tab(&tab.id);
                changed = true;
            }
        }
        if changed {
            self.tabs_changed(false, window, cx);
        }
    }

    /// Undo of the last Rename or Move: the files go back and the reference updates are reverted.
    fn undo_move(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Nothing to undo does nothing.
        let Some(undo) = self.files.last_move else { return };
        cx.spawn_in(window, async move |this, cx| {
            // The files are written before they move back.
            save_all(&this, cx).await?;
            let call = this.update(cx, |workbench, cx| {
                host_call(workbench.label.clone(), cx, move |host, label| async move {
                    ide_host::ops::refactor::refactor_undo_file_move(&host, &label, undo).await
                })
            })?;
            let outcome = call.await;
            this.update_in(cx, |workbench, window, cx| match outcome {
                Err(error) => workbench.notify(undo_failed_notification(&error.message), cx),
                Ok(outcome) => {
                    if workbench.files.last_move == Some(undo) {
                        workbench.files.last_move = None;
                        workbench.files.log.forget_move();
                    }
                    workbench.files_moved(&outcome.moves, window, cx);
                    let done = undone_notification(&mut cx.default_global::<Localization>().0, &outcome.conflicts);
                    workbench.notify(done, cx);
                    workbench.start_flow(rename_dialog_done(&outcome.moves), false, None, window, cx);
                }
            })
        })
        .detach();
    }

    // ---------------------------------------------------------------------------------------------
    // New

    /// The New popup for `folder` (the selection's folder when `None`); with `outside_tree` it was
    /// asked for in the editor. `pick` chooses its row of that kind at once. False without a folder.
    fn show_new_popup(
        &mut self,
        folder: Option<String>,
        outside_tree: bool,
        pick: Option<NewKind>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tree) = self.project_tree().cloned() else { return false };
        tree.read(cx).mirror_into(&mut self.files.state);
        let Some(folder) = folder.or_else(|| selection_target_folder(&self.files.state)) else { return false };
        let selected = crate::project_view::context_menu::MenuGroupContext::default();
        let contributed = self.project_view_contributed(&selected, Some(&folder), window, cx);
        let items = self
            .with_contributed_menu_context(contributed, cx, |ctx, _| new_menu_items(ctx, Some(&folder), outside_tree));
        let request = NewPopupRequest { at: (0.0, 0.0), items, seq: 0 };
        let actions = self.popup_actions(window, cx);
        let popup = cx.new(|cx| NewPopupView::new(&request, actions, cx));
        let in_tree = !outside_tree;
        let events = cx.subscribe_in(&popup, window, move |workbench, popup, event: &NewPopupEvent, window, cx| {
            workbench.new_popup_event(popup, event.clone(), in_tree, window, cx);
        });
        self.files.new_popup = Some(popup.downgrade());
        self.open_popup(&popup, events, window, cx);
        if let Some(kind) = pick {
            popup.update(cx, |popup, cx| popup.choose(kind, window, cx));
        }
        true
    }

    /// The New popup for the folder of the edited file; with `or_root`, the project root without one.
    fn show_new_popup_for_editor(&mut self, or_root: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let edited = self.active_file_path(cx).map(|path| parent_of(&path).to_owned());
        let root = || self.project_tree().map(|tree| tree.read(cx).root().to_owned());
        let Some(folder) = edited.or_else(|| or_root.then(root).flatten()) else { return false };
        self.show_new_popup(Some(folder), true, None, window, cx)
    }

    /// New File / New Directory: the New popup for the folder given (or the selection's), asking
    /// for the name of that kind straight away.
    fn new_item(&mut self, kind: NewKind, arg: Option<&Value>, window: &mut Window, cx: &mut Context<Self>) {
        self.with_layout(window, cx, |layout| layout.show(PROJECT_TOOL_WINDOW, false));
        if let Some(tree) = self.project_tree().cloned() {
            tree.read(cx).mirror_into(&mut self.files.state);
        }
        let folder = folder_from(&self.files.state, arg);
        self.show_new_popup(folder, false, Some(kind), window, cx);
    }

    fn new_popup_event(
        &mut self,
        popup: &Entity<NewPopupView>,
        event: NewPopupEvent,
        in_tree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            NewPopupEvent::Dismissed => self.close_popup(window, cx),
            NewPopupEvent::Run(run) => {
                self.close_popup(window, cx);
                match run {
                    PopupRun::Command(command) => self.run_project_command(command, Vec::new(), in_tree, window, cx),
                    PopupRun::Action { id, arg } => {
                        window.defer(cx, move |window, cx| {
                            execute_action_from(ActionSource::Menu, id, arg.as_ref(), window, cx);
                        });
                    }
                }
            }
            NewPopupEvent::Begin(command) => {
                let named = popup.downgrade();
                let started = self.with_view(in_tree, window, cx, |state, env| {
                    let nodes: Vec<Arc<ProjectNode>> = selected_nodes(state);
                    run_command(command, &node_refs(&nodes), state, env, None, &[])
                });
                let done: Done = Box::new(move |workbench, _, _, cx| {
                    let Some(crate::project_view::store::Editing::New { parent, .. }) = &workbench.files.state.editing
                    else {
                        return;
                    };
                    let siblings = workbench.files.state.listings.get(parent).into_iter().flatten();
                    let siblings = siblings.map(|entry| entry.name.clone()).collect();
                    let rule = NameRule { siblings, host_os: this_os() };
                    let _ = named.update(cx, |popup, cx| popup.set_name_rule(rule, cx));
                });
                self.start_flow(started, in_tree, Some(done), window, cx);
            }
            NewPopupEvent::Create { name } => {
                // The creation takes the name first: closing the popup then drops nothing.
                let started = self.with_view(in_tree, window, cx, |state, env| {
                    crate::project_view::operations::commit_editing(state, env, &name)
                });
                self.close_popup(window, cx);
                self.start_flow(started, in_tree, None, window, cx);
            }
            NewPopupEvent::Back => crate::project_view::operations::cancel_editing(&mut self.files.state),
        }
    }

    /// The popup slot closed `view`: a creation the New popup began is dropped.
    pub(super) fn files_popup_closed(&mut self, view: &AnyView) {
        let ours = self.files.new_popup.as_ref().and_then(WeakEntity::upgrade);
        if ours.is_some_and(|popup| AnyView::from(popup) == *view) {
            self.files.new_popup = None;
            crate::project_view::operations::cancel_editing(&mut self.files.state);
        }
    }

    /// What the New popup knows of the registered actions.
    fn popup_actions(&self, window: &Window, cx: &App) -> PopupActionSet {
        let Some(registry) = cx.try_global::<ActionRegistry>() else { return PopupActionSet::default() };
        let ctx = window_context(window, cx);
        let mut actions = PopupActionSet::default();
        for action in registry.all() {
            let id = action.meta.id.clone();
            if !is_action_enabled(&action.meta, &ctx) {
                actions.disabled.insert(id.clone());
            }
            actions.registered.insert(id, action.meta.clone());
        }
        actions.shortcuts = self.shortcuts(cx);
        actions
    }

    /// The keymap's shortcut of every registered action, by action id.
    fn shortcuts(&self, cx: &App) -> BTreeMap<String, String> {
        let (Some(registry), Some(keymap)) = (cx.try_global::<ActionRegistry>(), cx.try_global::<Keymap>()) else {
            return BTreeMap::new();
        };
        let mac = cfg!(target_os = "macos");
        let labels = registry.ids().filter_map(|id| Some((id.to_owned(), keymap.0.shortcut_label(id, mac)?)));
        labels.collect()
    }

    /// Builds what the Project view's menus are made from: the tree's state, the project, whether it is a Git
    /// repository, the registered actions and their shortcuts. No other feature contributes rows.
    pub(super) fn with_menu_context<R>(&self, cx: &App, build: impl FnOnce(&MenuContext<'_>, &App) -> R) -> R {
        self.with_contributed_menu_context(Contributed::default(), cx, build)
    }

    /// [`Self::with_menu_context`], with the groups other features contribute to the menus.
    fn with_contributed_menu_context<R>(
        &self,
        contributed: Contributed,
        cx: &App,
        build: impl FnOnce(&MenuContext<'_>, &App) -> R,
    ) -> R {
        let root = self.files.state.root.clone().unwrap_or_default();
        let name = self.files.state.name.clone();
        let actions: BTreeSet<String> = cx
            .try_global::<ActionRegistry>()
            .map(|registry| registry.ids().map(str::to_owned).collect())
            .unwrap_or_default();
        let shortcuts = self.shortcuts(cx);
        let ctx = MenuContext {
            state: &self.files.state,
            project: (!root.is_empty()).then_some(OpenProject { root: &root, name: &name, workspace: None }),
            is_git: self.vcs_state().is_some_and(|vcs| vcs.available),
            roots: &[],
            detected: &[],
            os: this_os(),
            mac: cfg!(target_os = "macos"),
            actions: &actions,
            shortcuts: &shortcuts,
            contributed: &contributed,
        };
        build(&ctx, cx)
    }

    // ---------------------------------------------------------------------------------------------
    // The tree's context menu

    /// The tree asks for its context menu: the rows the selection gets.
    pub(super) fn tree_context_menu(
        &mut self,
        _: &Entity<ProjectTree>,
        event: &ContextMenuRequested,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tree) = self.project_tree().cloned() else { return };
        tree.read(cx).mirror_into(&mut self.files.state);
        let nodes = selected_nodes(&self.files.state);
        cx.default_global::<Localization>();
        let selected = crate::project_view::context_menu::menu_group_context(&node_refs(&nodes));
        // The New submenu creates in the folder the menu's own rows target.
        let lead = nodes.first().map(|node| &**node);
        let new_in = crate::project_view::operations::target_folder(&self.files.state, lead);
        let contributed = self.project_view_contributed(&selected, new_in.as_deref(), window, cx);
        let items = cx.update_global::<Localization, _>(|localization, cx| {
            self.with_contributed_menu_context(contributed, cx, |ctx, _| {
                shown_items(&context_menu_items(ctx, &mut localization.0, &node_refs(&nodes)), ctx.actions)
            })
        });
        let position = event.position;
        self.show_menu(
            &items,
            position,
            move |workbench, command, window, cx| {
                workbench.run_project_command(command, nodes.clone(), true, window, cx)
            },
            window,
            cx,
        );
    }

    /// Carries out a row of the Project view's menus for `nodes` (the selection when empty).
    pub(super) fn run_project_command(
        &mut self,
        command: ProjectCommand,
        nodes: Vec<Arc<ProjectNode>>,
        in_tree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A New row of the menu asks for the name in the New popup.
        if let ProjectCommand::New { kind, parent, .. } = &command {
            self.show_new_popup(parent.clone(), !in_tree, Some(*kind), window, cx);
            return;
        }
        let root = self.project_tree().map(|tree| tree.read(cx).root().to_owned()).unwrap_or_default();
        let name = self.files.state.name.clone();
        let started = self.with_view(in_tree, window, cx, |state, env| {
            let nodes = if nodes.is_empty() { selected_nodes(state) } else { nodes };
            let project = Some(OpenProject { root: &root, name: &name, workspace: None });
            run_command(command, &node_refs(&nodes), state, env, project, &[])
        });
        self.start_flow(started, in_tree, None, window, cx);
    }

    // ---------------------------------------------------------------------------------------------
    // Rename, Move, Copy

    fn show_file_dialog(&mut self, request: FileRefactorRequest, window: &mut Window, cx: &mut Context<Self>) {
        let storage = cx.try_global::<UiStorage>();
        let options = match &request {
            FileRefactorRequest::Rename { .. } => refactor_options_key(RefactorLabel::Rename),
            _ => refactor_options_key(RefactorLabel::Move),
        };
        let options = load_refactor_options(storage.and_then(|storage| storage.get(&options)));
        let open_copy = storage.and_then(|storage| storage.get(COPY_OPEN_KEY)) == Some("true");
        if let Some(tree) = self.project_tree() {
            tree.read(cx).mirror_into(&mut self.files.state);
        }
        let listings = self.files.state.listings.iter();
        let listings =
            listings.map(|(folder, entries)| (folder.clone(), entries.iter().map(|e| e.name.clone()).collect()));
        let input = FileDialogInput {
            request,
            root: self.files.state.root.clone().unwrap_or_default(),
            host_os: this_os(),
            options,
            open_copy,
            listings: listings.collect(),
        };
        let dialog = cx.new(|cx| FileDialog::new(input, cx));
        let events = cx.subscribe_in(&dialog, window, Self::file_dialog_event);
        let focus = dialog.focus_handle(cx);
        self.open_file_dialog(dialog.into(), DialogKind::File, focus, events, window, cx);
    }

    fn file_dialog_event(
        &mut self,
        dialog: &Entity<FileDialog>,
        event: &FileDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            FileDialogEvent::Cancelled => {
                if let (Some(stop), Some(backend)) = (self.files.reference_search.take(), cx.try_global::<Backend>()) {
                    stop.stop(&backend.host);
                }
                self.close_file_dialog(window, cx);
            }
            FileDialogEvent::Refactor { label, moves, options } => {
                if cx.has_global::<UiStorage>() {
                    cx.global_mut::<UiStorage>().set(&refactor_options_key(label), options.stored());
                }
                dialog.update(cx, |dialog, cx| dialog.set_busy(true, cx));
                let clipboard = match dialog.read(cx).request() {
                    FileRefactorRequest::Move { clipboard, .. } => *clipboard,
                    _ => None,
                };
                let view = AnyView::from(dialog.clone());
                let stop = self.start_reference_search();
                cx.spawn_in(window, async move |this, cx| {
                    let status =
                        run_refactoring(this.clone(), cx, moves.clone(), options, label, Some(view.clone()), stop)
                            .await;
                    this.update_in(cx, |workbench, window, cx| {
                        workbench.close_dialog_of(&view, window, cx);
                        if !matches!(status, Ok(RefactorStatus::Done)) {
                            return;
                        }
                        let started = match label {
                            RefactorLabel::Rename => rename_dialog_done(&moves),
                            RefactorLabel::Move => workbench.with_view(true, window, cx, |state, env| {
                                move_dialog_done(state, env, clipboard, &moves)
                            }),
                        };
                        workbench.start_flow(started, true, None, window, cx);
                    })
                })
                .detach();
            }
            FileDialogEvent::Copy { paths, target, name, open_in_editor } => {
                if cx.has_global::<UiStorage>() {
                    cx.global_mut::<UiStorage>().set(COPY_OPEN_KEY, open_in_editor.to_string());
                }
                dialog.update(cx, |dialog, cx| dialog.set_busy(true, cx));
                let started = self.with_view(true, window, cx, |state, env| {
                    copy_dialog_confirmed(state, env, &paths, &target, name.as_deref(), open_in_editor)
                });
                let view = AnyView::from(dialog.clone());
                let weak = dialog.downgrade();
                let done: Done = Box::new(move |workbench, outcome, window, cx| {
                    if outcome == Outcome::Accepted(true) {
                        workbench.close_dialog_of(&view, window, cx);
                    } else {
                        let _ = weak.update(cx, |dialog, cx| dialog.set_busy(false, cx));
                    }
                });
                self.start_flow(started, true, Some(done), window, cx);
            }
            FileDialogEvent::Browse { current } => {
                let picked = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: None,
                });
                let weak = dialog.downgrade();
                cx.spawn_in(window, async move |_, cx| {
                    let picked = match picked.await {
                        Ok(Ok(Some(paths))) => paths.into_iter().next(),
                        Ok(Ok(None)) => None,
                        Ok(Err(error)) => {
                            tracing::warn!("project view: cannot pick a folder from {current}: {error:#}");
                            None
                        }
                        Err(_) => None,
                    };
                    if let Some(path) = picked {
                        let _ = weak.update(cx, |dialog, cx| dialog.set_target(&path.to_string_lossy(), cx));
                    }
                })
                .detach();
            }
        }
    }

    /// The refactoring preview of `edit`; the receiver gets the edit of the files kept, `None` when
    /// it was cancelled.
    fn show_preview(
        &mut self,
        title: String,
        edit: WorkspaceEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Option<WorkspaceEdit>> {
        let (sender, answer) = oneshot::channel();
        let root = self.files.state.root.clone().unwrap_or_default();
        let preview = cx.new(|cx| RefactoringPreview::new(PreviewInput { title, edit, root }, cx));
        let mut sender = Some(sender);
        let events =
            cx.subscribe_in(&preview, window, move |workbench, _, event: &RefactoringPreviewEvent, window, cx| {
                workbench.close_file_dialog(window, cx);
                let kept = match event {
                    RefactoringPreviewEvent::Confirmed(edit) => Some(edit.clone()),
                    RefactoringPreviewEvent::Cancelled => None,
                };
                if let Some(sender) = sender.take() {
                    // The refactoring may have ended meanwhile.
                    let _ = sender.send(kept);
                }
            });
        // The lines of the occurrences, from the editors' text or the disk.
        for path in preview.read(cx).file_paths() {
            let at = path.clone();
            let read = host_call(self.label.clone(), cx, move |host, label| async move {
                ide_host::ops::fs::read_text(&host, &label, PathBuf::from(at)).await
            });
            let weak = preview.downgrade();
            cx.spawn(async move |_, cx| match read.await {
                Ok(text) => {
                    let lines = text.lines().map(str::to_owned).collect();
                    let _ = weak.update(cx, |preview, cx| preview.set_lines(&path, lines, cx));
                }
                Err(error) => tracing::debug!("refactoring preview: cannot read {path}: {}", error.message),
            })
            .detach();
        }
        let focus = preview.focus_handle(cx);
        self.open_file_dialog(preview.into(), DialogKind::Preview, focus, events, window, cx);
        answer
    }

    /// Asks `ask` for the flow `id`, which `reply` turns the answer into.
    fn ask_for_flow(
        &mut self,
        id: u64,
        ask: AskRequest,
        kind: DialogKind,
        reply: fn(&DialogAnswer) -> Reply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| PromptDialog::ask(ask, window, cx));
        let events = cx.subscribe_in(&dialog, window, move |workbench, _, event: &PromptDialogEvent, window, cx| {
            let PromptDialogEvent::Closed(answer) = event;
            workbench.close_file_dialog(window, cx);
            workbench.resume_flow(id, reply(answer), window, cx);
        });
        let focus = dialog.focus_handle(cx);
        self.open_file_dialog(dialog.into(), kind, focus, events, window, cx);
    }

    // ---------------------------------------------------------------------------------------------
    // Copy Path

    /// Copy Path/Reference… for the edited file at the caret: its path forms at once, the reference
    /// once it is resolved.
    fn show_copy_path_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor().cloned() else { return };
        let (path, line) = {
            let editor = editor.read(cx);
            (editor.path().to_string_lossy().into_owned(), editor.caret_position().0)
        };
        let root = self.project_tree().map(|tree| tree.read(cx).root().to_owned());
        let name = self.files.state.name.clone();
        let project = root.as_deref().map(|root| OpenProject { root, name: &name, workspace: None });
        let items = copy_path_items(&path, line, project, &[], &self.shortcuts(cx));
        let popup = cx.new(|cx| CopyPathPopup::new(items, cx));
        let events = cx.subscribe_in(&popup, window, |workbench, _, event: &CopyPathEvent, window, cx| {
            workbench.close_popup(window, cx);
            if let CopyPathEvent::Copy { action, text } = event {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                let hint = copied_hint(&mut cx.default_global::<Localization>().0, action, text);
                workbench.run_flow_action("editor.showHint", serde_json::json!({ "text": hint }), window, cx);
            }
        });
        self.open_popup(&popup, events, window, cx);
        // A stale answer finds the popup gone.
        let reference = self.caret_reference(&editor, cx);
        let weak = popup.downgrade();
        cx.spawn(async move |_, cx| {
            let reference = reference.await;
            let _ = weak.update(cx, |popup, cx| popup.set_reference(Some(reference), cx));
        })
        .detach();
    }

    /// Edit › Copy Reference: puts the qualified name of what the caret is on (or `given`, when the
    /// action is run with a text) on the clipboard and in the editors' clipboard history, and says so.
    fn copy_reference(&mut self, given: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let reference = match given {
            Some(text) => Task::ready(text),
            None => {
                let Some(editor) = self.editor().cloned() else { return };
                self.caret_reference(&editor, cx)
            }
        };
        cx.spawn_in(window, async move |workbench, cx| {
            let text = reference.await;
            let _ = workbench.update_in(cx, |workbench, window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                cx.default_global::<EditorClipboard>().0.record(&text, false);
                let hint = copied_hint(&mut cx.default_global::<Localization>().0, REFERENCE, &text);
                workbench.run_flow_action("editor.showHint", serde_json::json!({ "text": hint }), window, cx);
            });
        })
        .detach();
    }

    /// The reference of the caret's place in `editor`: the chain of declarations around an
    /// identifier under the caret, read from the document's outline, else the file and line.
    fn caret_reference(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> Task<String> {
        let (path, (line, col), doc, language, text, on_name) = {
            let editor = editor.read(cx);
            let path = editor.path().to_string_lossy().into_owned();
            let text = editor.text();
            let on_name = reference_offset(&text, editor.caret(), editor.caret()).is_some();
            (path, editor.caret_position(), editor.doc(), editor.language().to_string(), text, on_name)
        };
        let root = self.project_tree().map(|tree| tree.read(cx).root().to_owned());
        let outline = host_call(self.label.clone(), cx, move |host, label| async move {
            ide_host::ops::doc::doc_outline(&host, &label, doc).await
        });
        cx.spawn(async move |_, _| {
            // Off an identifier the reference is the caret's place.
            let chain = match outline.await {
                Ok(_) if !on_name => Vec::new(),
                Ok(items) => symbol_chain(&items, Pos { line, col }),
                Err(error) => {
                    tracing::debug!("copy reference: no outline of {path}: {}", error.message);
                    Vec::new()
                }
            };
            let input = ReferenceInput {
                language: &language,
                path: &path,
                root: root.as_deref(),
                text: &text,
                chain: &chain,
                line,
            };
            qualified_reference(&input)
        })
    }

    /// The Copy Path popup that is open.
    pub fn copy_path_popup(&self) -> Option<Entity<CopyPathPopup>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    /// The New popup that is open.
    pub fn new_popup(&self) -> Option<Entity<NewPopupView>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    // ---------------------------------------------------------------------------------------------
    // The dialog layer

    fn open_file_dialog(
        &mut self,
        view: AnyView,
        kind: DialogKind,
        focus: FocusHandle,
        events: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Another dialog in its place gives the keyboard back to where it was before the first.
        let restore = match self.files.dialog.take() {
            Some(previous) => previous.restore,
            None => window.focused(cx),
        };
        window.focus(&focus, cx);
        self.files.dialog = Some(DialogSlot { view, kind, restore, _events: events });
        cx.notify();
    }

    fn close_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.files.dialog.take() else { return };
        if let Some(target) = dialog.restore.or_else(|| self.editor().map(|editor| editor.focus_handle(cx))) {
            window.focus(&target, cx);
        }
        cx.notify();
    }

    /// Closes the dialog layer if it still shows `view`.
    fn close_dialog_of(&mut self, view: &AnyView, window: &mut Window, cx: &mut Context<Self>) {
        if self.files.dialog.as_ref().is_some_and(|dialog| dialog.view == *view) {
            self.close_file_dialog(window, cx);
        }
    }

    /// The dialog of a file operation that is open.
    pub fn file_dialog<V: 'static>(&self) -> Option<Entity<V>> {
        self.files.dialog.as_ref().and_then(|dialog| dialog.view.clone().downcast().ok())
    }

    /// The dialog of a file operation over the window, while one is open.
    pub(super) fn file_dialog_layer(&self) -> Option<AnyElement> {
        let view = self.files.dialog.as_ref()?.view.clone();
        let layer = div()
            .id("file-dialog-layer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .child(view);
        Some(gpui_kit::deferred(layer).with_priority(1).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::{Range, SymbolKind};

    /// A symbol spanning `lines`, named on the first of them at columns 6..9.
    fn item(name: &str, kind: SymbolKind, lines: (u32, u32), children: Vec<OutlineItem>) -> OutlineItem {
        let range = Range { start: Pos { line: lines.0, col: 0 }, end: Pos { line: lines.1, col: 1 } };
        let selection_range = Range { start: Pos { line: lines.0, col: 6 }, end: Pos { line: lines.0, col: 9 } };
        OutlineItem { name: name.to_owned(), detail: None, kind, range, selection_range, children, visibility: None }
    }

    // Not from a spec: the reference names the symbols down to the one named at the caret, a method by its bare
    // name and without the package; nothing on a symbol's body.
    #[test]
    fn the_reference_chain_ends_at_the_symbol_named_at_the_caret() {
        let method = item("run(int)", SymbolKind::Method, (2, 4), Vec::new());
        let class = item("A", SymbolKind::Class, (1, 9), vec![method]);
        let outline = [item("p", SymbolKind::Package, (0, 10), vec![class])];
        assert_eq!(symbol_chain(&outline, Pos { line: 2, col: 7 }), ["A", "run"]);
        assert_eq!(symbol_chain(&outline, Pos { line: 1, col: 7 }), ["A"]);
        assert!(symbol_chain(&outline, Pos { line: 3, col: 0 }).is_empty());
    }
}
