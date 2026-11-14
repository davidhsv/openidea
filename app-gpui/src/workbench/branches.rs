//! The Git branches of a project window: the branch widget of the title bar, the Branches popup and
//! the operations they start (checkout, new branch, rename, delete, merge, rebase, update, pull,
//! push, fetch), carried out on the backend.
//!
//! The window keeps one [`VcsState`] for its project, read off the window's thread when the project
//! opens, after every operation started here and on the backend's version control and working tree
//! events. The widget and the popup are fed from it.
//!
//! The popup lives in the popup slot: its submenus and options list are drawn inside it. The dialogs
//! the operations ask (branch names, confirmations, questions, Push) are a layer of their own below
//! the popup slot, one at a time: only their own buttons and Escape close them, so an answer is never
//! lost to a stray click. An operation waits for its dialog's answer; a dialog dropped without one
//! answers Cancel.

use super::Workbench;
use super::services_window::on_workbench;
use crate::backend::Backend;
use crate::platform::actions::{ActionRegistry, ActionSource, execute_action_from};
use crate::platform::context::set_context;
use crate::platform::i18n::{I18n, Localization, Param};
use crate::platform::local_time::now_ms;
use crate::platform::storage::UiStorage;
use crate::vcs::branch_widget_view::{
    BranchWidget, BranchWidgetEvent, BranchWidgetState, MergeRebaseEvent, MergeRebaseState, MergeRebaseWidget,
};
use crate::vcs::branches::{BranchTreeOptions, RECENT_LIMIT, RepoEntry, current_repo_for, push_recent};
use crate::vcs::branches_popup::{BranchesCommand, BranchesPopup, BranchesPopupEvent, BranchesPopupInput, RepoTarget};
use crate::vcs::prompt_dialogs::{
    CANCEL, DialogAnswer, PromptDialog, PromptDialogEvent, PushDialog, PushDialogEvent, PushDialogInput, PushRepo,
    PushRequest,
};
use crate::vcs_more::remote_results::outgoing_push;
use crate::vcs_ops::branch_menu::{BranchCommand, pull_into};
use crate::vcs_ops::branch_ops::{
    BranchFollowUp, CheckoutStep, CheckoutTarget, DeleteBranchFlow, DeleteStep, branch_ref, checkout_retry,
    checkout_step, checkout_target, conflict_actions, create_branch, delete_remote_branch, deleted_branch_notice,
    favorites_key, merge_titles, new_branch_prompt, parse_names, rebase_titles, recent_key, rename_branch,
    rename_branch_prompt, renamed_recents, toggled_favorite,
};
use crate::vcs_ops::dialogs::{AskRequest, ConfirmRequest};
use crate::vcs_ops::operations::{Control, InProgress, control_label, control_texts, in_progress, none_in_progress};
use crate::vcs_ops::ops::{
    ConsoleKind, VcsConsole, all_repos, describe_operation, is_update, op_failed, op_started, op_succeeded,
    remote_console_line, remote_success_body, remote_success_title, remote_titles, updated_from, updated_title,
    vcs_notice,
};
use crate::vcs_ops::remote_ui::{RepoPush, outgoing_query};
use crate::vcs_ops::store::{
    RefreshData, RefreshQueue, RefreshRequest, RefreshStart, SelectedRepo, VcsState, touches_worktree,
};
use crate::workbench::notifications::{NotificationLevel, NotificationRequest};
use futures::channel::oneshot;
use gpui_kit::{
    AnyElement, AnyView, App, AsyncWindowContext, Context, Entity, FocusHandle, Focusable, IntoElement, Subscription,
    Task, WeakEntity, Window, div, prelude::*,
};
use ide_core::{IdeEvent, IdeResult};
use ide_host::Host;
use ide_host::ops::Channel;
use ide_host::ops::vcs::{self as git, GitOperation, RemoteOp, VcsRoot};
use ide_vcs::{Branch, CheckoutOptions, LogQuery};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the backend's events rest before the repository is read: version control events, and
/// changes of the working tree.
const VCS_EVENT_DELAY: Duration = Duration::from_millis(200);
const FS_EVENT_DELAY: Duration = Duration::from_millis(250);

use super::conflicts::RESOLVE_CONFLICTS;

/// UI storage key of the Branches popup's gear options.
pub(crate) const OPTIONS_KEY: &str = "openidea.vcs.branchesPopup.options";

/// The version control state of a project window, with the branch widget that shows it.
pub(super) struct VcsSlot {
    state: VcsState,
    queue: RefreshQueue,
    /// Who waits for which refresh run.
    waiters: Vec<(u64, oneshot::Sender<()>)>,
    refreshing: Option<Task<()>>,
    /// The Git command of the window that runs, by its number: no read starts meanwhile, as a read
    /// writes the refreshed index back and would take the index lock the command needs.
    git_running: Option<u64>,
    /// A read asked for while a command ran: it starts once the command ends.
    read_deferred: bool,
    /// The operation marked as running in `state.remote_busy`, by its number (see [`BusyMark`]).
    busy_mark: Option<u64>,
    /// Who waits for the running command or read to end before running a command of its own.
    gate_waiters: Vec<oneshot::Sender<()>>,
    /// The read a burst of backend events asks for, once they stop.
    debounce: Option<Task<()>>,
    /// Tags of the selected repository.
    tags: Vec<String>,
    /// Root → its branches, when the project has several repositories.
    repo_branches: HashMap<String, Vec<Branch>>,
    /// Recently checked-out branches, newest first.
    recent: Vec<String>,
    /// The user's favourite branches; `None` while the defaults apply.
    favorites: Option<Vec<String>>,
    /// The gear options are stored; the groups opened in the popups last for the session.
    options: BranchTreeOptions,
    multi_expanded: HashMap<String, bool>,
    /// The Git console of the window's operations.
    console: VcsConsole,
    /// What the buttons of the notifications raised here do, by their command.
    follow_ups: Vec<(String, BranchFollowUp)>,
    widget: Entity<BranchWidget>,
    /// Resolve, Continue and Abort while an operation is stopped.
    merge_widget: Entity<MergeRebaseWidget>,
    _widget_events: [Subscription; 2],
    /// What reached the repository's reading, for a test that times out to print.
    #[cfg(test)]
    trace: Vec<String>,
    /// How many debounces the window armed, to tell in the trace which one fired.
    #[cfg(test)]
    debounces: u64,
}

/// The dialog an operation waits on.
pub(super) struct VcsDialog {
    view: AnyView,
    /// What had the keyboard before the dialog opened.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

/// The gear options of the Branches popup as stored: the groups opened are not.
fn load_options(stored: Option<&str>) -> BranchTreeOptions {
    let mut options = BranchTreeOptions::default();
    let Some(Value::Object(map)) = stored.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
        return options;
    };
    let flag = |key: &str| map.get(key).and_then(Value::as_bool);
    options.group_by_prefix = flag("groupByPrefix").unwrap_or(options.group_by_prefix);
    options.show_recent = flag("showRecent").unwrap_or(options.show_recent);
    options.show_tags = flag("showTags").unwrap_or(options.show_tags);
    options
}

fn stored_options(options: &BranchTreeOptions) -> String {
    serde_json::json!({
        "groupByPrefix": options.group_by_prefix,
        "showRecent": options.show_recent,
        "showTags": options.show_tags,
    })
    .to_string()
}

/// A list of branch names under `key`.
fn store_names(key: &str, names: &[String], cx: &mut App) {
    match serde_json::to_string(names) {
        Ok(json) => cx.default_global::<UiStorage>().set(key, json),
        Err(error) => tracing::warn!("cannot store {key}: {error}"),
    }
}

fn stored_names(key: &str, cx: &App) -> Option<Vec<String>> {
    parse_names(cx.try_global::<UiStorage>().and_then(|storage| storage.get(key)))
}

fn repo_path(repo: Option<&str>) -> Option<PathBuf> {
    repo.map(PathBuf::from)
}

fn root_text(root: &VcsRoot) -> String {
    root.root.0.to_string_lossy().into_owned()
}

/// Runs `work` on the backend with the host and the window's label; the failure is its message.
fn host_call<T, F, W>(
    label: String,
    cx: &App,
    work: W,
) -> impl Future<Output = Result<T, String>> + 'static + use<T, F, W>
where
    T: Send + 'static,
    F: Future<Output = IdeResult<T>> + Send + 'static,
    W: FnOnce(Arc<Host>, String) -> F + Send + 'static,
{
    let call = cx.try_global::<Backend>().map(|backend| backend.with_host(move |host| work(host, label)));
    async move {
        match call {
            None => Err("The backend is not running".to_owned()),
            Some(call) => match call.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(error.message),
                Err(error) => Err(error.to_string()),
            },
        }
    }
}

/// What one refresh read.
struct Snapshot {
    roots: Vec<VcsRoot>,
    status: Vec<ide_vcs::FileStatus>,
    selected: Result<SelectedRepo, String>,
    tags: Vec<String>,
    repo_branches: HashMap<String, Vec<Branch>>,
}

async fn read_snapshot(host: Arc<Host>, label: String, repo: Option<PathBuf>) -> IdeResult<Snapshot> {
    let roots = git::vcs_roots(&host, &label)?;
    let status = git::status(&host, &label).await?;
    let selected = match git::vcs_branch_info(&host, &label, repo.clone()).await {
        Ok(branch) => match git::vcs_branches(&host, &label, repo.clone()).await {
            Ok(branches) => Ok(SelectedRepo { branch, branches: Some(branches) }),
            Err(error) => Err(error.message),
        },
        Err(error) => Err(error.message),
    };
    // Tags are an extra of the popup: a repository whose tags cannot be read shows none.
    let tags = git::vcs_tags(&host, &label, repo).await.unwrap_or_default();
    let mut repo_branches = HashMap::new();
    if roots.len() > 1 {
        for root in &roots {
            // A repository whose branches cannot be listed shows as loading in the popup.
            if let Ok(branches) = git::vcs_branches(&host, &label, Some(root.root.0.clone())).await {
                repo_branches.insert(root_text(root), branches);
            }
        }
    }
    Ok(Snapshot { roots, status, selected, tags, repo_branches })
}

/// What a network operation came to in the repositories it ran in.
#[derive(Default)]
struct RemoteRun {
    /// Why it failed: the error of one repository, or every failed repository's.
    error: Option<String>,
    /// Per push that carried commits: the upstream and how many.
    pushed: Vec<(String, usize)>,
    /// Repositories an update brought commits into, with the files and commits it brought.
    updated: usize,
    files: usize,
    commits: usize,
    /// The operation's output, line by line.
    lines: Vec<String>,
}

/// Where the branch meets the branch an update brings commits from (the start and the end of what
/// came in), else HEAD.
async fn published_tip(host: &Host, label: &str, repo: Option<PathBuf>, op: &RemoteOp) -> Option<String> {
    if let Some(tracked) = updated_from(op) {
        let base = git::vcs_merge_base(host, label, repo.clone(), "HEAD".to_owned(), tracked).await;
        if let Ok(Some(base)) = base {
            return Some(base);
        }
    }
    git::vcs_branch_info(host, label, repo).await.ok().and_then(|info| info.head_commit)
}

/// Runs `op` in each of `repos` (with its name when there are several) in turn; every repository is
/// tried and the failures are told together.
async fn remote_in_repos(
    host: Arc<Host>,
    label: String,
    op: RemoteOp,
    repos: Vec<(Option<PathBuf>, Option<String>)>,
) -> IdeResult<RemoteRun> {
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut run = RemoteRun::default();
    let mut failures = Vec::new();
    for (repo, name) in repos {
        let before = if is_update(&op) { published_tip(&host, &label, repo.clone(), &op).await } else { None };
        let branch = git::vcs_branch_info(&host, &label, repo.clone()).await.ok();
        let outgoing = outgoing_push(&op, branch.as_ref());
        let sink = Arc::clone(&lines);
        let channel = Channel::new(move |line: String| match sink.lock() {
            Ok(mut lines) => lines.push(line),
            Err(poisoned) => poisoned.into_inner().push(line),
        });
        let request = Backend::next_request_id();
        match git::vcs_remote(&host, &label, repo.clone(), request, op.clone(), channel).await {
            Ok(()) => {
                run.pushed.extend(outgoing);
                let after = if before.is_some() { published_tip(&host, &label, repo.clone(), &op).await } else { None };
                if let (Some(from), Some(to)) = (before, after).clone()
                    && from != to
                {
                    run.updated += 1;
                    let files = git::vcs_diff_revisions(&host, &label, repo.clone(), from.clone(), to.clone()).await;
                    run.files += files.map(|files| files.len()).unwrap_or_default();
                    let query = LogQuery { range: Some(format!("{from}..{to}")), limit: 1000, ..LogQuery::default() };
                    run.commits +=
                        git::vcs_log(&host, &label, repo, query).await.map(|log| log.len()).unwrap_or_default();
                }
            }
            Err(error) => match name {
                Some(name) => failures.push(format!("{name}: {}", error.message)),
                None => {
                    run.error = Some(error.message);
                    break;
                }
            },
        }
    }
    if !failures.is_empty() {
        run.error = Some(failures.join("\n"));
    }
    run.lines = match lines.lock() {
        Ok(lines) => lines.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    Ok(run)
}

/// The Push dialog's repositories: each with its branch, its outgoing commits and its remotes. A
/// repository whose branch cannot be read is left out.
async fn push_repos(host: Arc<Host>, label: String, roots: Vec<(String, String)>) -> IdeResult<(Vec<PushRepo>, bool)> {
    let mut repos = Vec::new();
    let mut has_hook = false;
    for (root, name) in roots {
        let path = Some(PathBuf::from(&root));
        let Ok(branch) = git::vcs_branch_info(&host, &label, path.clone()).await else { continue };
        // A repository without remotes lists none to pick from.
        let remotes = git::vcs_remotes(&host, &label, path.clone()).await.unwrap_or_default();
        let commits = if branch.head.is_some() {
            let query = outgoing_query(branch.upstream.as_deref());
            let log = |range: Option<&str>, limit: u32| LogQuery {
                branch: Some(query.branch.to_owned()),
                range: range.map(str::to_owned),
                limit,
                ..LogQuery::default()
            };
            // An upstream deleted on the remote no longer resolves: recent history is shown instead.
            match git::vcs_log(&host, &label, path.clone(), log(query.range, query.limit)).await {
                Ok(commits) => commits,
                Err(_) => git::vcs_log(&host, &label, path.clone(), log(None, 50)).await?,
            }
        } else {
            Vec::new()
        };
        // Without an answer about the hook, Run Git hooks is not offered.
        has_hook |= git::vcs_has_hook(&host, &label, path, "pre-push".to_owned()).await.unwrap_or(false);
        let remotes = remotes.into_iter().map(|remote| remote.name).collect();
        repos.push(PushRepo { push: RepoPush { root, branch, commits }, name, remotes });
    }
    Ok((repos, has_hook))
}

/// Registers the actions of the branch widget and the Branches popup.
pub(super) fn register_actions(registry: &mut ActionRegistry) {
    registry.handle("vcs.branches", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.toggle_branches_popup(window, cx))
    });
    registry.handle("vcs.newBranch", |call, window, cx| {
        let repos = call.arg.and_then(|arg| arg.get("repos")).and_then(Value::as_array).map_or(0, Vec::len);
        if repos > 1 {
            anyhow::bail!("New Branch in several repositories at once is not available");
        }
        on_workbench(window, cx, |workbench, window, cx| workbench.start_new_branch(None, None, window, cx))
    });
    registry.handle("vcs.checkoutRevision", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.start_checkout_revision(window, cx))
    });
    registry.handle("vcs.fetch", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.start_fetch(window, cx))
    });
    registry.handle("vcs.push", |call, window, cx| {
        let branch = call.arg.and_then(|arg| arg.get("branch")).and_then(Value::as_str).map(str::to_owned);
        // A commit id, or a commit object with one.
        let revision = call.arg.and_then(|arg| arg.get("revision")).and_then(|revision| {
            revision.as_str().or_else(|| revision.get("id").and_then(Value::as_str)).map(str::to_owned)
        });
        on_workbench(window, cx, |workbench, window, cx| workbench.start_push(branch, revision, window, cx))
    });
}

/// Runs action `id` once this update is over: it may reach for the workbench.
pub(super) fn run_action_later(id: String, arg: Option<Value>, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        if !execute_action_from(ActionSource::Other, &id, arg.as_ref(), window, cx) {
            tracing::info!("{id} is not available in this window");
        }
    });
}

/// The end of an operation started from the window: it only fails when the window went away.
pub(super) fn finished(result: anyhow::Result<()>) {
    if let Err(error) = result {
        tracing::debug!("a Git operation ended with its window: {error:#}");
    }
}

pub(super) type This = WeakEntity<Workbench>;

/// Runs `work` on the backend for the window's project.
pub(super) async fn call<T, F>(
    this: &This,
    cx: &mut AsyncWindowContext,
    work: impl FnOnce(Arc<Host>, String) -> F + Send + 'static,
) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: Future<Output = IdeResult<T>> + Send + 'static,
{
    let call = this.update(cx, |workbench, cx| host_call(workbench.label.clone(), cx, work))?;
    Ok(call.await)
}

/// `f` with the UI language.
pub(super) fn i18n<R>(this: &This, cx: &mut AsyncWindowContext, f: impl FnOnce(&mut I18n) -> R) -> anyhow::Result<R> {
    this.update(cx, |_, cx| f(&mut cx.default_global::<Localization>().0))
}

pub(super) fn notify(this: &This, cx: &mut AsyncWindowContext, request: NotificationRequest) -> anyhow::Result<()> {
    this.update(cx, |workbench, cx| workbench.notify(request, cx))
}

/// Reads the repository again and waits until what it read is shown.
pub(super) async fn refresh(this: &This, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let refreshed = this.update(cx, |workbench, cx| workbench.request_vcs_refresh(cx))?;
    // A refresh dropped with its window settles nothing more.
    let _ = refreshed.await;
    Ok(())
}

/// The number of the next Git command, across windows and repository states.
static GIT_COMMANDS: AtomicU64 = AtomicU64::new(1);

/// A Git command holding the window's gate. The gate opens when [`git_command_ended`] is called
/// with it, or when it is dropped (its future was dropped before the command ended): a release
/// task waits for the guard to go and opens the gate on the next turn, so a command that never
/// ends cannot hold every later command and read of the window back.
#[must_use = "the gate stays closed only while the guard lives"]
pub(super) struct GitCommand {
    number: u64,
    /// Dropping it wakes the release task.
    _release: oneshot::Sender<()>,
}

/// Waits until neither a read of the repository nor another Git command of the window runs, then
/// holds reads off until the returned guard ends or is dropped. Every command of the window's
/// operations runs while it holds a guard. `None` when the window has no repository state.
pub(super) async fn git_command_starts(this: &This, cx: &mut AsyncWindowContext) -> anyhow::Result<Option<GitCommand>> {
    loop {
        let wait = this.update(cx, |workbench, cx| {
            let Some(slot) = workbench.vcs.as_mut() else { return Ok(None) };
            if slot.git_running.is_none() && !slot.queue.in_flight() {
                // A late release of an earlier command never opens this one's gate.
                let number = GIT_COMMANDS.fetch_add(1, Ordering::Relaxed);
                slot.git_running = Some(number);
                let (release, released) = oneshot::channel::<()>();
                cx.spawn(async move |workbench, cx| {
                    // Ended or dropped: either way the command no longer runs.
                    let _ = released.await;
                    // A window that is gone has no gate to open.
                    let _ = workbench.update(cx, |workbench, cx| workbench.end_git_command(number, cx));
                })
                .detach();
                return Ok(Some(GitCommand { number, _release: release }));
            }
            let (sender, receiver) = oneshot::channel();
            slot.gate_waiters.push(sender);
            Err(receiver)
        })?;
        match wait {
            Ok(command) => return Ok(command),
            // A waiter dropped with its window finds no slot on the next turn.
            Err(wait) => {
                let _ = wait.await;
            }
        }
    }
}

/// An operation of the window marked as running (`remote_busy`): no other operation starts while it
/// lives. The mark goes when the operation clears it, or when the guard is dropped (its future was
/// dropped before the operation ended): a release task waits for the guard to go and clears the mark
/// on the next turn, so an operation that never ends cannot make every later one answer that another
/// is in progress.
#[must_use = "the operation is marked as running only while the guard lives"]
pub(super) struct BusyMark {
    /// Dropping it wakes the release task.
    _release: oneshot::Sender<()>,
}

/// Another Git operation of the window runs.
#[derive(Debug)]
pub(super) struct OperationRuns;

/// Marks an operation showing `progress` as running in `slot`, until the returned guard goes.
fn mark_busy(slot: &mut VcsSlot, progress: String, cx: &mut Context<Workbench>) -> BusyMark {
    // A late release of an earlier mark never clears this one.
    let number = GIT_COMMANDS.fetch_add(1, Ordering::Relaxed);
    slot.state.remote_busy = Some(progress);
    slot.busy_mark = Some(number);
    let (release, released) = oneshot::channel::<()>();
    cx.spawn(async move |workbench, cx| {
        // Cleared or dropped: either way the operation no longer runs.
        let _ = released.await;
        // A window that is gone has nothing to clear.
        let _ = workbench.update(cx, |workbench, cx| workbench.clear_busy_mark(number, cx));
    })
    .detach();
    BusyMark { _release: release }
}

/// The operation marked in `slot` ended: another one can start.
fn clear_busy(slot: &mut VcsSlot) {
    slot.state.remote_busy = None;
    slot.busy_mark = None;
}

/// The command of `command` ended: a read asked for meanwhile starts now.
pub(super) fn git_command_ended(
    this: &This,
    cx: &mut AsyncWindowContext,
    command: Option<GitCommand>,
) -> anyhow::Result<()> {
    let Some(command) = command else { return Ok(()) };
    this.update(cx, |workbench, cx| workbench.end_git_command(command.number, cx))
}

/// Saves the open documents so git sees what the editors hold; a failure leaves git with what is on
/// disk, as saving is best effort here.
pub(super) async fn save_all(this: &This, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let saving = this.update(cx, |workbench, cx| workbench.save_all(cx))?;
    if let Err(error) = saving.await {
        tracing::warn!("cannot save the documents before a Git operation: {error}");
    }
    Ok(())
}

/// Shows the dialog `build` makes and waits for its answer; a dialog dropped without one answers Cancel.
async fn ask_dialog(
    this: &This,
    cx: &mut AsyncWindowContext,
    build: impl FnOnce(&mut Window, &mut Context<PromptDialog>) -> PromptDialog + 'static,
) -> anyhow::Result<DialogAnswer> {
    let answer = this.update_in(cx, |workbench, window, cx| workbench.show_prompt_dialog(build, window, cx))?;
    Ok(answer.await.unwrap_or_else(|_| DialogAnswer { button: CANCEL.to_owned(), ..DialogAnswer::default() }))
}

/// The state of the window's repository, as it is now.
pub(super) fn vcs_state<R>(
    this: &This,
    cx: &mut AsyncWindowContext,
    f: impl FnOnce(&VcsState) -> R,
) -> anyhow::Result<R> {
    this.update(cx, |workbench, _| match workbench.vcs.as_ref() {
        Some(slot) => f(&slot.state),
        None => f(&VcsState::default()),
    })
}

/// A local Git operation: logged in the console, `success` reported, a failure reported as
/// "`title` failed" (with Resolve Conflicts or Abort when `conflicts` matter to it), then the
/// repository read again. A stop on conflicts opens them for resolving instead, when the conflicts
/// dialog is there. `None` when it failed.
pub(super) async fn run_op<T, F>(
    this: &This,
    cx: &mut AsyncWindowContext,
    title: &str,
    success: Option<String>,
    conflicts: bool,
    work: impl FnOnce(Arc<Host>, String) -> F + Send + 'static,
) -> anyhow::Result<Option<T>>
where
    T: Send + 'static,
    F: Future<Output = IdeResult<T>> + Send + 'static,
{
    this.update(cx, |workbench, _| {
        if let Some(slot) = workbench.vcs.as_mut() {
            op_started(&mut slot.console, title, now_ms());
        }
        #[cfg(test)]
        workbench.vcs_trace(format!("operation {title} starts"));
    })?;
    let command = git_command_starts(this, cx).await?;
    let result = call(this, cx, work).await;
    git_command_ended(this, cx, command)?;
    let result = result?;
    #[cfg(test)]
    this.update(cx, |workbench, _| workbench.vcs_trace(format!("operation {title} ended: ok={}", result.is_ok())))?;
    refresh(this, cx).await?;
    let title = title.to_owned();
    this.update_in(cx, |workbench, window, cx| {
        let resolver = cx.try_global::<ActionRegistry>().is_some_and(|registry| registry.contains(RESOLVE_CONFLICTS));
        let slot = workbench.vcs.as_mut()?;
        let notice = match &result {
            Ok(_) => op_succeeded(&mut slot.console, success.as_deref(), now_ms()),
            // A merge or rebase stopped on conflicts is not an error: they open for resolving.
            Err(message) if conflicts && resolver && !slot.state.conflicted_files().is_empty() => {
                slot.console.log(ConsoleKind::Err, message, now_ms());
                run_action_later(RESOLVE_CONFLICTS.to_owned(), None, window, cx);
                None
            }
            Err(message) => {
                let i18n = &mut cx.default_global::<Localization>().0;
                let mut notice = op_failed(&mut slot.console, &title, message, true, i18n, now_ms());
                if let Some(notice) = notice.as_mut().filter(|_| conflicts) {
                    let operation = slot.state.branch.as_ref().and_then(|branch| branch.operation.as_deref());
                    notice.actions = conflict_actions(message, slot.state.conflicted_files().len(), operation);
                }
                notice
            }
        };
        if let Some(notice) = notice {
            workbench.notify(notice, cx);
        }
        result.ok()
    })
}

/// A network operation in `repos`: the progress shows until it ends, then what it did or why it
/// failed. Only one runs at a time. Whether it succeeded.
pub(super) async fn run_remote(
    this: &This,
    cx: &mut AsyncWindowContext,
    op: RemoteOp,
    detail: Option<String>,
    repos: Vec<Option<String>>,
) -> anyhow::Result<bool> {
    let (progress, done) = remote_titles(&op);
    let started = this.update(cx, |workbench, cx| {
        let slot = workbench.vcs.as_mut()?;
        if slot.state.remote_busy.is_some() {
            let warning = vcs_notice(NotificationLevel::Warning, "Another remote operation is in progress");
            workbench.notify(warning, cx);
            return None;
        }
        let progress = cx.default_global::<Localization>().0.tx(progress);
        let mark = mark_busy(slot, progress.clone(), cx);
        slot.console.log(ConsoleKind::Cmd, &remote_console_line(&op, detail.as_deref()), now_ms());
        let mut notice = vcs_notice(NotificationLevel::Info, format!("{progress}…"));
        notice.key = Some("vcs.remote".to_owned());
        notice.body = detail.clone();
        notice.sticky = Some(true);
        workbench.notify(notice, cx);
        Some(mark)
    })?;
    let Some(_mark) = started else { return Ok(false) };
    let named: Vec<(Option<PathBuf>, Option<String>)> = vcs_state(this, cx, |state| {
        repos
            .iter()
            .map(|repo| {
                let name = repo.as_deref().filter(|_| repos.len() > 1).map(|root| state.repo_name(root).to_owned());
                (repo_path(repo.as_deref()), name)
            })
            .collect()
    })?;
    let command = git_command_starts(this, cx).await?;
    let ran = {
        let op = op.clone();
        call(this, cx, move |host, label| remote_in_repos(host, label, op, named)).await
    };
    git_command_ended(this, cx, command)?;
    let ran = ran?;
    let run = ran.unwrap_or_else(|error| RemoteRun { error: Some(error), ..RemoteRun::default() });
    refresh(this, cx).await?;
    let ok = run.error.is_none();
    this.update(cx, |workbench, cx| {
        let i18n = &mut cx.default_global::<Localization>().0;
        let mut notice = match &run.error {
            Some(error) => {
                let mut notice = vcs_notice(NotificationLevel::Error, format!("{done} failed"));
                notice.body = Some(error.clone());
                notice
            }
            None if is_update(&op) => {
                vcs_notice(NotificationLevel::Info, updated_title(run.updated, run.files, run.commits, i18n))
            }
            None => {
                let mut notice = vcs_notice(NotificationLevel::Info, remote_success_title(&op, &run.pushed, i18n));
                let last = run.lines.iter().rev().map(|line| line.trim()).find(|line| !line.is_empty());
                notice.body = last.and_then(remote_success_body).map(str::to_owned);
                notice
            }
        };
        notice.key = Some("vcs.remote".to_owned());
        if let Some(slot) = workbench.vcs.as_mut() {
            clear_busy(slot);
            for line in &run.lines {
                slot.console.log(ConsoleKind::Out, line, now_ms());
            }
            if let Some(error) = &run.error {
                slot.console.log(ConsoleKind::Err, error, now_ms());
            }
        }
        workbench.notify(notice, cx);
        // The buttons that waited for the operation can be pressed again.
        workbench.vcs_state_changed(cx);
    })?;
    Ok(ok)
}

/// What a completed checkout reports.
pub(super) enum Success {
    /// "Checked out" the branch now current (else this name).
    Head(String),
    Fixed(String),
}

/// Checks out `rev` as IDEA does: local changes in the way ask about Smart Checkout, and a remote
/// branch whose local namesake has commits of its own asks whether to rebase or drop them. Whether it
/// switched.
pub(super) async fn checkout(
    this: &This,
    cx: &mut AsyncWindowContext,
    rev: String,
    repo: Option<String>,
    success: Success,
) -> anyhow::Result<bool> {
    save_all(this, cx).await?;
    let mut options = CheckoutOptions::default();
    loop {
        let title = i18n(this, cx, |i18n| i18n.tx_built(&["Checkout ", ""], &[Param::from(rev.as_str())]))?;
        let (target, path) = (rev.clone(), repo_path(repo.as_deref()));
        let work = move |host: Arc<Host>, label: String| async move {
            git::vcs_checkout(&host, &label, path, target, false, Some(options)).await
        };
        let Some(outcome) = run_op(this, cx, &title, None, false, work).await? else { return Ok(false) };
        let text = match &success {
            Success::Fixed(text) => text.clone(),
            Success::Head(name) => vcs_state(this, cx, |state| {
                let head = state.branch.as_ref().and_then(|branch| branch.head.clone());
                format!("Checked out {}", head.unwrap_or_else(|| name.clone()))
            })?,
        };
        let step = i18n(this, cx, |i18n| checkout_step(&outcome, &text, false, i18n))?;
        match step {
            CheckoutStep::Switched { notice, .. } => {
                if let Some(notice) = notice {
                    notify(this, cx, notice)?;
                }
                return Ok(true);
            }
            CheckoutStep::Failed(notice) => {
                notify(this, cx, notice)?;
                return Ok(false);
            }
            CheckoutStep::Ask(request) => {
                let answer = ask(this, cx, request).await?;
                match checkout_retry(options, &answer.ask_answer()) {
                    Some(retry) => options = retry,
                    None => return Ok(false),
                }
            }
        }
    }
}

pub(super) async fn ask(this: &This, cx: &mut AsyncWindowContext, request: AskRequest) -> anyhow::Result<DialogAnswer> {
    ask_dialog(this, cx, move |window, cx| PromptDialog::ask(request, window, cx)).await
}

pub(super) async fn confirm(this: &This, cx: &mut AsyncWindowContext, request: ConfirmRequest) -> anyhow::Result<bool> {
    let answer = ask_dialog(this, cx, move |window, cx| PromptDialog::confirm(request, window, cx)).await?;
    Ok(answer.confirmed())
}

/// Puts `name` first among the recent branches.
fn remember_branch(this: &This, cx: &mut AsyncWindowContext, name: Option<String>) -> anyhow::Result<()> {
    let Some(name) = name.filter(|name| !name.is_empty()) else { return Ok(()) };
    this.update(cx, |workbench, cx| workbench.remember_branch(&name, cx))
}

/// The branches of `repo`: the selected repository's as listed, another's read now.
async fn branches_of(this: &This, cx: &mut AsyncWindowContext, repo: Option<String>) -> anyhow::Result<Vec<Branch>> {
    let (selected, listed) = vcs_state(this, cx, |state| {
        let selected = repo.as_deref().is_none_or(|repo| Some(repo) == state.current_repo_root());
        (selected, state.branches_loaded.then(|| state.branches.clone()))
    })?;
    if let (true, Some(listed)) = (selected, listed) {
        return Ok(listed);
    }
    let path = repo_path(repo.as_deref());
    let read = call(this, cx, move |host, label| async move { git::vcs_branches(&host, &label, path).await }).await?;
    Ok(read.unwrap_or_default())
}

/// The current branch of `repo`.
fn head_of(state: &VcsState, repo: Option<&str>) -> Option<String> {
    if repo.is_none_or(|repo| Some(repo) == state.current_repo_root()) {
        return state.branch.as_ref().and_then(|branch| branch.head.clone());
    }
    repo.and_then(|repo| state.heads.get(repo)).and_then(|head| head.head.clone())
}

async fn checkout_branch(
    this: This,
    cx: &mut AsyncWindowContext,
    name: String,
    repo: Option<String>,
) -> anyhow::Result<()> {
    let (branches, previous) = vcs_state(&this, cx, |state| (state.branches.clone(), head_of(state, repo.as_deref())))?;
    // A local namesake tracking another branch: a new local name is asked for instead.
    if checkout_target(&name, &branches) == CheckoutTarget::NewBranch {
        return new_branch(this, cx, Some(name), repo).await;
    }
    if checkout(&this, cx, name.clone(), repo.clone(), Success::Head(name)).await? {
        remember_branch(&this, cx, previous)?;
        let now = vcs_state(&this, cx, |state| head_of(state, repo.as_deref()))?;
        remember_branch(&this, cx, now)?;
    }
    Ok(())
}

/// New Branch from HEAD (or `from`) in `repo`: asks for a name, creates the branch and, unless the
/// user unchecks it, checks it out.
async fn new_branch(
    this: This,
    cx: &mut AsyncWindowContext,
    from: Option<String>,
    repo: Option<String>,
) -> anyhow::Result<()> {
    let branches = branches_of(&this, cx, repo.clone()).await?;
    let previous = vcs_state(&this, cx, |state| head_of(state, repo.as_deref()))?;
    let prompt = i18n(&this, cx, |i18n| new_branch_prompt(from.as_deref(), previous.as_deref(), &branches, i18n))?;
    let answer = ask_dialog(&this, cx, move |window, cx| PromptDialog::new_branch(prompt, window, cx)).await?;
    let Some(result) = answer.prompt_result() else { return Ok(()) };
    let create = i18n(&this, cx, |i18n| create_branch(&result, from.as_deref(), i18n))?;
    if create.checkout {
        save_all(&this, cx).await?;
    }
    let (name, start, checkout, overwrite) =
        (create.name.clone(), create.from.clone(), create.checkout, create.overwrite);
    let path = repo_path(repo.as_deref());
    let work = move |host: Arc<Host>, label: String| async move {
        git::vcs_create_branch(&host, &label, path, name, start, checkout, Some(overwrite)).await
    };
    let created = run_op(&this, cx, &create.title, Some(create.success.clone()), false, work).await?;
    if created.is_some() && create.checkout {
        remember_branch(&this, cx, previous)?;
        remember_branch(&this, cx, Some(create.name))?;
    }
    Ok(())
}

async fn checkout_revision(this: This, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let answer = ask_dialog(&this, cx, PromptDialog::checkout_revision).await?;
    let Some(result) = answer.prompt_result() else { return Ok(()) };
    let repo = vcs_state(&this, cx, |state| state.repo.clone())?;
    let success = Success::Fixed(format!("Checked out {}", result.value));
    checkout(&this, cx, result.value, repo, success).await?;
    Ok(())
}

async fn rename(this: This, cx: &mut AsyncWindowContext, name: String, repo: Option<String>) -> anyhow::Result<()> {
    let branches = branches_of(&this, cx, repo.clone()).await?;
    let prompt = i18n(&this, cx, |i18n| rename_branch_prompt(&name, &branches, i18n))?;
    let answer = ask_dialog(&this, cx, move |window, cx| PromptDialog::rename_branch(prompt, window, cx)).await?;
    let Some(result) = answer.prompt_result() else { return Ok(()) };
    let (to, title, success) = i18n(&this, cx, |i18n| rename_branch(&name, &result, i18n))?;
    let (from, target, path) = (name.clone(), to.clone(), repo_path(repo.as_deref()));
    let work = move |host: Arc<Host>, label: String| async move {
        git::vcs_rename_branch(&host, &label, path, from, target).await
    };
    if run_op(&this, cx, &title, Some(success), false, work).await?.is_some() {
        this.update(cx, |workbench, cx| {
            let Some(slot) = workbench.vcs.as_mut() else { return };
            slot.recent = renamed_recents(&slot.recent, &name, &to);
            let key = recent_key(slot.state.root.as_deref());
            store_names(&key, &slot.recent, cx);
        })?;
    }
    Ok(())
}

/// Delete Branch as IDEA does it: no question unless a worktree holds the branch; the notification
/// says what was discarded and offers Restore.
async fn delete(this: This, cx: &mut AsyncWindowContext, name: String, repo: Option<String>) -> anyhow::Result<()> {
    let mut flow = DeleteBranchFlow::new(&name, repo.as_deref());
    let path = repo_path(repo.as_deref());
    let listed =
        call(&this, cx, move |host, label| async move { git::vcs_worktrees(&host, &label, path).await }).await?;
    let project_root = vcs_state(&this, cx, |state| state.root.clone())?;
    let mut step =
        i18n(&this, cx, |i18n| flow.worktrees_listed(listed.ok().as_deref(), project_root.as_deref(), i18n))?;
    loop {
        step = match step {
            DeleteStep::Confirm(request) => {
                let ok = confirm(&this, cx, request).await?;
                i18n(&this, cx, |i18n| flow.confirmed(ok, i18n))?
            }
            DeleteStep::RemoveWorktree { path: worktree, force, title } => {
                let path = repo_path(repo.as_deref());
                let work = move |host: Arc<Host>, label: String| async move {
                    git::vcs_remove_worktree(&host, &label, path, PathBuf::from(worktree), force).await
                };
                let removed = match title {
                    Some(title) => run_op(&this, cx, &title, None, false, work).await?.ok_or_else(String::new),
                    None => call(&this, cx, work).await?,
                };
                i18n(&this, cx, |i18n| {
                    flow.worktree_removed(removed.as_ref().map(|_| ()).map_err(String::as_str), i18n)
                })?
            }
            DeleteStep::DeleteBranch { title } => {
                let (branch, path) = (name.clone(), repo_path(repo.as_deref()));
                let work = move |host: Arc<Host>, label: String| async move {
                    git::vcs_delete_branch(&host, &label, path, branch).await
                };
                let Some(deleted) = run_op(&this, cx, &title, None, false, work).await? else { return Ok(()) };
                let branches = branches_of(&this, cx, repo.clone()).await?;
                let notice =
                    i18n(&this, cx, |i18n| deleted_branch_notice(&name, repo.as_deref(), &deleted, &branches, i18n))?;
                this.update(cx, |workbench, cx| {
                    if let Some(slot) = workbench.vcs.as_mut() {
                        slot.follow_ups.retain(|(command, _)| !notice.follow_ups.iter().any(|(own, _)| own == command));
                        slot.follow_ups.extend(notice.follow_ups.iter().cloned());
                    }
                    workbench.notify(notice.request.clone(), cx);
                })?;
                return Ok(());
            }
            DeleteStep::Abort => return Ok(()),
        };
    }
}

async fn delete_remote(
    this: This,
    cx: &mut AsyncWindowContext,
    name: String,
    repo: Option<String>,
) -> anyhow::Result<()> {
    let (request, op, detail) = i18n(&this, cx, |i18n| delete_remote_branch(&name, i18n))?;
    if confirm(&this, cx, request).await? {
        run_remote(&this, cx, op, Some(detail), vec![repo]).await?;
    }
    Ok(())
}

/// Merge `branch` (as `reference`) into the current branch, or rebase the current branch onto it.
async fn merge_or_rebase(
    this: &This,
    cx: &mut AsyncWindowContext,
    branch: String,
    reference: String,
    repo: Option<String>,
    rebase: bool,
) -> anyhow::Result<()> {
    let current = vcs_state(this, cx, |state| head_of(state, repo.as_deref()))?;
    let (title, success) = i18n(this, cx, |i18n| {
        if rebase {
            rebase_titles(&branch, current.as_deref(), i18n)
        } else {
            merge_titles(&branch, current.as_deref(), i18n)
        }
    })?;
    save_all(this, cx).await?;
    let path = repo_path(repo.as_deref());
    let work = move |host: Arc<Host>, label: String| async move {
        if rebase {
            git::vcs_rebase(&host, &label, path, reference).await
        } else {
            git::vcs_merge(&host, &label, path, reference).await
        }
    };
    run_op(this, cx, &title, Some(success), true, work).await?;
    Ok(())
}

/// Checkout and Rebase onto Current: checks out `name`, then rebases it onto the branch that was current.
async fn checkout_and_rebase(
    this: This,
    cx: &mut AsyncWindowContext,
    name: String,
    repo: Option<String>,
) -> anyhow::Result<()> {
    let Some(current) = vcs_state(&this, cx, |state| head_of(state, repo.as_deref()))? else { return Ok(()) };
    if !checkout(&this, cx, name.clone(), repo.clone(), Success::Fixed(format!("Checked out {name}"))).await? {
        return Ok(());
    }
    remember_branch(&this, cx, Some(current.clone()))?;
    remember_branch(&this, cx, Some(name))?;
    let reference = branch_ref(&current, false);
    merge_or_rebase(&this, cx, current, reference, repo, true).await
}

/// Update for a branch other than the current one: fast-forwards it to its upstream.
async fn update_branch(
    this: This,
    cx: &mut AsyncWindowContext,
    name: String,
    repo: Option<String>,
) -> anyhow::Result<()> {
    let branches = branches_of(&this, cx, repo.clone()).await?;
    let branch = branches.into_iter().find(|branch| !branch.remote && branch.name == name);
    if branch.as_ref().is_some_and(|branch| branch.current) {
        return this.update_in(cx, |_, window, cx| run_action_later("vcs.update".to_owned(), None, window, cx));
    }
    let Some(upstream) = branch.and_then(|branch| branch.upstream).filter(|upstream| !upstream.is_empty()) else {
        let warning = vcs_notice(NotificationLevel::Warning, format!("Branch '{name}' has no tracked branch"));
        return notify(&this, cx, warning);
    };
    let op = RemoteOp::UpdateBranch { branch: name.clone() };
    run_remote(&this, cx, op, Some(format!("{name} ← {upstream}")), vec![repo]).await?;
    Ok(())
}

async fn pull(
    this: This,
    cx: &mut AsyncWindowContext,
    remote_branch: String,
    rebase: bool,
    repo: Option<String>,
) -> anyhow::Result<()> {
    let current = vcs_state(&this, cx, |state| head_of(state, repo.as_deref()))?;
    let Some((op, detail)) = pull_into(&remote_branch, rebase, current.as_deref()) else { return Ok(()) };
    run_remote(&this, cx, op, Some(detail), vec![repo]).await?;
    Ok(())
}

async fn fetch(this: This, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let roots = vcs_state(&this, cx, |state| state.roots.clone())?;
    run_remote(&this, cx, RemoteOp::Fetch, None, all_repos(&roots)).await?;
    Ok(())
}

/// Push: the dialog lists each repository with something to push and its outgoing commits; each
/// checked one is pushed, even when another fails, and the failures are summed up.
pub(super) async fn push(this: This, cx: &mut AsyncWindowContext, revision: Option<String>) -> anyhow::Result<()> {
    refresh(&this, cx).await?;
    let (roots, selected) = vcs_state(&this, cx, |state| {
        let roots: Vec<(String, String)> =
            state.roots.iter().map(|root| (root.clone(), state.repo_name(root).to_owned())).collect();
        (roots, state.current_repo_root().map(str::to_owned))
    })?;
    let listed = call(&this, cx, move |host, label| push_repos(host, label, roots)).await?;
    let (repos, has_hook) = match listed {
        Ok(listed) => listed,
        Err(error) => {
            let mut failure = vcs_notice(NotificationLevel::Error, "Push failed");
            failure.body = Some(error);
            return notify(&this, cx, failure);
        }
    };
    let protected = this.update(cx, |workbench, cx| {
        crate::platform::settings::effective(&workbench.label, cx).vcs.git.protected_branches
    })?;
    let input = PushDialogInput { repos, selected, has_hook, protected_branches: protected, revision };
    let chosen = this.update_in(cx, |workbench, window, cx| workbench.show_push_dialog(input, window, cx))?;
    // A dialog dropped without an answer pushes nothing.
    let Ok(Some(requests)) = chosen.await else { return Ok(()) };
    let mut failed = Vec::new();
    for PushRequest { root, op, detail, .. } in &requests {
        if !run_remote(&this, cx, op.clone(), Some(detail.clone()), vec![Some(root.clone())]).await? {
            failed.push(vcs_state(&this, cx, |state| state.repo_name(root).to_owned())?);
        }
    }
    if !failed.is_empty() && requests.len() > 1 {
        let title = if failed.len() < requests.len() { "Push partially failed" } else { "Push failed" };
        let not_pushed = failed.join(", ");
        let body = i18n(&this, cx, |i18n| {
            i18n.tx_built(&["Not pushed: ", ". See the Git console for details."], &[Param::from(not_pushed.as_str())])
        })?;
        let mut notice = vcs_notice(NotificationLevel::Error, title);
        notice.key = Some("vcs.remote".to_owned());
        notice.body = Some(body);
        notify(&this, cx, notice)?;
    }
    Ok(())
}

/// Key of the notification of a long Git operation: its progress, then what came of it.
pub(super) const OPERATION_KEY: &str = "vcs.operation";

/// A long local Git operation and what it says while it runs and after.
pub(super) struct GitRun {
    pub(super) op: GitOperation,
    /// Shown while it runs (English source text).
    pub(super) progress: &'static str,
    pub(super) success: String,
    /// What a rebase that stops for editing reports instead of `success`.
    pub(super) stopped: Option<String>,
    pub(super) failure: String,
    /// A failure that left conflicts opens them for resolving instead of an error notice.
    pub(super) conflicts_instead: bool,
}

/// A local Git operation that may take a while: its progress shows until it ends, then what it did
/// or why it failed. Only one runs at a time. Whether it succeeded.
pub(super) async fn run_git_operation(this: &This, cx: &mut AsyncWindowContext, run: GitRun) -> anyhow::Result<bool> {
    let GitRun { op, progress, success, stopped, failure, conflicts_instead } = run;
    let started = this.update(cx, |workbench, cx| {
        let slot = workbench.vcs.as_mut()?;
        if slot.state.remote_busy.is_some() {
            let warning = vcs_notice(NotificationLevel::Warning, "Another Git operation is in progress");
            workbench.notify(warning, cx);
            return None;
        }
        Some((slot.state.repo.clone(), slot.state.branch.as_ref().and_then(|branch| branch.operation.clone())))
    })?;
    let Some((repo, before)) = started else { return Ok(false) };
    save_all(this, cx).await?;
    let _mark = this.update(cx, |workbench, cx| {
        let slot = workbench.vcs.as_mut()?;
        let progress = cx.default_global::<Localization>().0.tx(progress);
        let mark = mark_busy(slot, progress.clone(), cx);
        slot.console.log(ConsoleKind::Cmd, &format!("git {}", describe_operation(&op, before.as_deref())), now_ms());
        let mut notice = vcs_notice(NotificationLevel::Info, format!("{progress}…"));
        notice.key = Some(OPERATION_KEY.to_owned());
        notice.sticky = Some(true);
        workbench.notify(notice, cx);
        Some(mark)
    })?;
    let command = git_command_starts(this, cx).await?;
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&lines);
    let path = repo_path(repo.as_deref());
    let ran = call(this, cx, move |host, label| async move {
        let channel = Channel::new(move |line: String| match sink.lock() {
            Ok(mut lines) => lines.push(line),
            Err(poisoned) => poisoned.into_inner().push(line),
        });
        git::vcs_operation(&host, &label, path, Backend::next_request_id(), op, channel).await
    })
    .await;
    git_command_ended(this, cx, command)?;
    let ran = ran?;
    refresh(this, cx).await?;
    let output = match lines.lock() {
        Ok(lines) => lines.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let ok = ran.is_ok();
    this.update_in(cx, |workbench, window, cx| {
        let resolver = cx.try_global::<ActionRegistry>().is_some_and(|registry| registry.contains(RESOLVE_CONFLICTS));
        let Some(slot) = workbench.vcs.as_mut() else { return };
        clear_busy(slot);
        for line in &output {
            slot.console.log(ConsoleKind::Out, line, now_ms());
        }
        let rebasing = in_progress(slot.state.branch.as_ref().and_then(|branch| branch.operation.as_deref()))
            == Some(InProgress::Rebase);
        let notice = match &ran {
            Ok(()) => {
                let text = match stopped {
                    Some(stopped) if rebasing => stopped,
                    _ => success,
                };
                slot.console.log(ConsoleKind::Info, &text, now_ms());
                Some(vcs_notice(NotificationLevel::Info, text))
            }
            Err(message) => {
                slot.console.log(ConsoleKind::Err, message, now_ms());
                // An operation stopped on conflicts is not an error: they open for resolving.
                if conflicts_instead && resolver && !slot.state.conflicted_files().is_empty() {
                    run_action_later(RESOLVE_CONFLICTS.to_owned(), None, window, cx);
                    None
                } else {
                    let mut notice = vcs_notice(NotificationLevel::Error, failure);
                    notice.body = Some(message.clone());
                    Some(notice)
                }
            }
        };
        match notice {
            Some(notice) => workbench.notify(NotificationRequest { key: Some(OPERATION_KEY.to_owned()), ..notice }, cx),
            None => workbench.notifications.expire_notification(OPERATION_KEY),
        }
        workbench.vcs_state_changed(cx);
    })?;
    Ok(ok)
}

/// Continue, Skip or Abort of the operation `expected` stopped in the selected repository.
pub(super) async fn control_operation(
    this: This,
    cx: &mut AsyncWindowContext,
    expected: InProgress,
    control: Control,
) -> anyhow::Result<()> {
    refresh(&this, cx).await?;
    let (op, conflicts) = vcs_state(&this, cx, |state| {
        let op = in_progress(state.branch.as_ref().and_then(|branch| branch.operation.as_deref()));
        let repo = state.current_repo_root();
        // Only the selected repository's conflicts hold it up: others may be in their own merge.
        let conflicts =
            state.conflicted_files().iter().filter(|file| repo.is_none() || state.repo_of(&file.path) == repo).count();
        (op, conflicts)
    })?;
    if op != Some(expected) {
        return notify(&this, cx, vcs_notice(NotificationLevel::Info, none_in_progress(expected)));
    }
    if control == Control::Continue && conflicts > 0 {
        notify(&this, cx, vcs_notice(NotificationLevel::Warning, "Resolve all conflicts before continuing"))?;
        return this.update_in(cx, |_, window, cx| run_action_later(RESOLVE_CONFLICTS.to_owned(), None, window, cx));
    }
    let (progress, success) = control_texts(expected, control);
    let label = control_label(expected, control);
    let failure = i18n(&this, cx, |i18n| i18n.tx_built(&["", " failed"], &[Param::from(label.as_str())]))?;
    let git_op = match control {
        Control::Continue => GitOperation::Continue,
        Control::Skip => GitOperation::Skip,
        Control::Abort => GitOperation::Abort,
    };
    // Continue and Skip that stop on the next conflict open them, as Abort cannot.
    let conflicts_instead = control != Control::Abort;
    let run = GitRun { op: git_op, progress, success: success.to_owned(), stopped: None, failure, conflicts_instead };
    run_git_operation(&this, cx, run).await?;
    Ok(())
}

/// Restore and Delete Tracked Branch of the "Deleted branch" notification.
async fn run_follow_up(this: This, cx: &mut AsyncWindowContext, follow_up: BranchFollowUp) -> anyhow::Result<()> {
    match follow_up {
        BranchFollowUp::Restore { repo, deleted, title, success } => {
            let path = repo_path(repo.as_deref());
            let work = move |host: Arc<Host>, label: String| async move {
                git::vcs_restore_branch(&host, &label, path, deleted).await
            };
            run_op(&this, cx, &title, Some(success), false, work).await?;
        }
        BranchFollowUp::DeleteTracked { branch, repo } => delete_remote(this, cx, branch, repo).await?,
        BranchFollowUp::ViewCommits { .. } => {
            tracing::info!("the commits of a deleted branch have no window to show them in");
        }
    }
    Ok(())
}

impl Workbench {
    /// The branch widget of the title bar, while a project is open.
    pub fn branch_widget(&self) -> Option<&Entity<BranchWidget>> {
        self.project_tree().and(self.vcs.as_ref()).map(|slot| &slot.widget)
    }

    /// The merge and rebase buttons of the title bar, while an operation is stopped or conflicts are
    /// left.
    pub fn merge_rebase_widget(&self, cx: &App) -> Option<&Entity<MergeRebaseWidget>> {
        let slot = self.project_tree().and(self.vcs.as_ref())?;
        Some(&slot.merge_widget).filter(|widget| widget.read(cx).is_needed())
    }

    /// The version control state of the window's project.
    pub fn vcs_state(&self) -> Option<&VcsState> {
        self.vcs.as_ref().map(|slot| &slot.state)
    }

    /// The Git console, the operation in progress, the pending reads and what reached them, for a
    /// test whose wait runs out.
    #[cfg(test)]
    pub(super) fn vcs_report(&self) -> String {
        let Some(slot) = self.vcs.as_ref() else { return "no repository slot".to_owned() };
        let state = &slot.state;
        let console: Vec<String> =
            slot.console.lines().iter().map(|line| format!("  {:?} {}", line.kind, line.text)).collect();
        let trace: Vec<String> = slot.trace.iter().rev().take(80).rev().map(|line| format!("  {line}")).collect();
        format!(
            "busy={:?} operation={:?} available={} loading={} error={:?} status={} version={} queue={:?} \
             reading={} debounce={} git command={:?} read deferred={}\nconsole:\n{}\ntrace:\n{}",
            state.remote_busy,
            state.branch.as_ref().and_then(|branch| branch.operation.as_deref()),
            state.available,
            state.loading,
            state.error,
            state.status.len(),
            state.version,
            slot.queue,
            slot.refreshing.is_some(),
            slot.debounce.is_some(),
            slot.git_running,
            slot.read_deferred,
            console.join("\n"),
            trace.join("\n"),
        )
    }

    /// Whether a Git command of the window holds the gate.
    #[cfg(test)]
    pub(super) fn git_gate_closed(&self) -> bool {
        self.vcs.as_ref().is_some_and(|slot| slot.git_running.is_some())
    }

    #[cfg(test)]
    fn vcs_trace(&mut self, line: String) {
        if let Some(slot) = self.vcs.as_mut() {
            slot.trace.push(format!("{:.0} {line}", now_ms()));
        }
    }

    /// The tags of the selected repository and the favourite branches as stored (`None`: the
    /// defaults).
    pub(super) fn vcs_tags_and_favorites(&self) -> (Vec<String>, Option<Vec<String>>) {
        self.vcs.as_ref().map_or((Vec::new(), None), |slot| (slot.tags.clone(), slot.favorites.clone()))
    }

    /// The Git console of the window's operations.
    pub fn vcs_console(&self) -> Option<&VcsConsole> {
        self.vcs.as_ref().map(|slot| &slot.console)
    }

    /// Writes `text` to the window's Git console.
    pub(super) fn log_vcs_console(&mut self, kind: ConsoleKind, text: &str) {
        if let Some(slot) = self.vcs.as_mut() {
            slot.console.log(kind, text, now_ms());
        }
    }

    /// Marks a Git operation the window does not run itself (gh's checkout of a pull request) as
    /// running, as the window's own runners do: no other operation starts meanwhile, until
    /// [`Self::end_outside_git_operation`] or until the mark is dropped. [`OperationRuns`] (with a
    /// warning) while another one runs; no mark when the project has no repository to guard.
    pub(super) fn begin_outside_git_operation(
        &mut self,
        progress: &str,
        cx: &mut Context<Self>,
    ) -> Result<Option<BusyMark>, OperationRuns> {
        let Some(slot) = self.vcs.as_mut() else { return Ok(None) };
        if slot.state.remote_busy.is_some() {
            let warning = vcs_notice(NotificationLevel::Warning, "Another Git operation is in progress");
            self.notify(warning, cx);
            return Err(OperationRuns);
        }
        Ok(Some(mark_busy(slot, progress.to_owned(), cx)))
    }

    /// The operation [`Self::begin_outside_git_operation`] marked has ended.
    pub(super) fn end_outside_git_operation(&mut self, mark: Option<BusyMark>) {
        if let (Some(slot), Some(_)) = (self.vcs.as_mut(), mark) {
            clear_busy(slot);
        }
    }

    /// The Branches popup, while it is open.
    pub fn branches_popup(&self) -> Option<Entity<BranchesPopup>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    /// The dialog a Git operation waits on, when it is a prompt, a confirmation or a question.
    pub fn vcs_prompt(&self) -> Option<Entity<PromptDialog>> {
        self.vcs_dialog.as_ref().and_then(|dialog| dialog.view.clone().downcast().ok())
    }

    /// The Push dialog, while it is open.
    pub fn push_dialog(&self) -> Option<Entity<PushDialog>> {
        self.vcs_dialog.as_ref().and_then(|dialog| dialog.view.clone().downcast().ok())
    }

    /// The dialog of the dialog layer, when it is a `V`.
    pub(super) fn vcs_dialog_view<V: 'static>(&self) -> Option<Entity<V>> {
        self.vcs_dialog.as_ref().and_then(|dialog| dialog.view.clone().downcast().ok())
    }

    /// Follows the project just opened: the widget goes in the title bar and the repository is read.
    pub(super) fn mount_vcs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_vcs();
        let Some(root) = self.project_root(cx).map(|root| root.to_string_lossy().into_owned()) else { return };
        let mut state = VcsState::default();
        state.reset_for_project(Some(&root), self.has_repository(cx));
        let widget = cx.new(|_| BranchWidget::new(BranchWidgetState::default()));
        let events = cx.subscribe_in(&widget, window, |_, _, event: &BranchWidgetEvent, window, cx| {
            let BranchWidgetEvent::TogglePopup = event;
            window.defer(cx, |window, cx| {
                execute_action_from(ActionSource::Toolbar, "vcs.branches", None, window, cx);
            });
        });
        let merge_widget = cx.new(|_| MergeRebaseWidget::new(MergeRebaseState::default()));
        let controls =
            cx.subscribe_in(&merge_widget, window, |_, _, event: &MergeRebaseEvent, window, cx| match *event {
                MergeRebaseEvent::ResolveConflicts => run_action_later(RESOLVE_CONFLICTS.to_owned(), None, window, cx),
                MergeRebaseEvent::Control(op, control) => cx
                    .spawn_in(window, async move |this, cx| finished(control_operation(this, cx, op, control).await))
                    .detach(),
            });
        let options = load_options(cx.try_global::<UiStorage>().and_then(|storage| storage.get(OPTIONS_KEY)));
        self.vcs = Some(VcsSlot {
            recent: stored_names(&recent_key(Some(&root)), cx).unwrap_or_default(),
            favorites: stored_names(&favorites_key(Some(&root)), cx),
            state,
            queue: RefreshQueue::default(),
            waiters: Vec::new(),
            refreshing: None,
            git_running: None,
            read_deferred: false,
            busy_mark: None,
            gate_waiters: Vec::new(),
            debounce: None,
            tags: Vec::new(),
            repo_branches: HashMap::new(),
            options,
            multi_expanded: HashMap::new(),
            console: VcsConsole::default(),
            follow_ups: Vec::new(),
            widget,
            merge_widget,
            _widget_events: [events, controls],
            #[cfg(test)]
            trace: Vec::new(),
            #[cfg(test)]
            debounces: 0,
        });
        self.mount_conflicts(window);
        self.mount_shelf(window, cx);
        self.refresh_vcs(cx);
    }

    /// The project goes: its state, its widget and the dialog it asked go with it.
    pub(super) fn drop_vcs(&mut self) {
        self.vcs = None;
        self.vcs_dialog = None;
        self.log_actions = Default::default();
        self.drop_conflicts();
        self.drop_shelf();
    }

    /// Whether the window's project holds a Git repository.
    fn has_repository(&self, cx: &App) -> bool {
        cx.try_global::<Backend>()
            .and_then(|backend| backend.host.project(&self.label).ok())
            .is_some_and(|project| !project.repos().is_empty())
    }

    /// A backend event that may change what the repository shows.
    pub(super) fn vcs_backend_event(&mut self, event: &IdeEvent, cx: &mut Context<Self>) {
        let Some(slot) = self.vcs.as_mut() else { return };
        let delay = match event {
            IdeEvent::VcsChanged { .. } => Some(VCS_EVENT_DELAY),
            IdeEvent::FsChanged { created, modified, removed } => {
                let paths: Vec<String> = created
                    .iter()
                    .chain(modified)
                    .chain(removed)
                    .map(|path| path.0.to_string_lossy().into_owned())
                    .collect();
                (slot.state.available && touches_worktree(&paths)).then_some(FS_EVENT_DELAY)
            }
            _ => None,
        };
        #[cfg(test)]
        {
            let what = match event {
                IdeEvent::FsChanged { created, modified, removed } => {
                    Some(format!("fs created={created:?} modified={modified:?} removed={removed:?}"))
                }
                IdeEvent::VcsChanged { .. } => Some("vcs changed".to_owned()),
                _ => None,
            };
            if let Some(what) = what {
                self.vcs_trace(format!("event {what} delay={delay:?}"));
            }
        }
        let Some(slot) = self.vcs.as_mut() else { return };
        let Some(delay) = delay else { return };
        if !slot.state.available {
            // A repository found below a project that was not one.
            if !self.has_repository(cx) {
                return;
            }
            let Some(slot) = self.vcs.as_mut() else { return };
            let root = slot.state.root.clone();
            slot.state.reset_for_project(root.as_deref(), true);
            self.refresh_vcs(cx);
            return;
        }
        #[cfg(test)]
        let number = {
            slot.debounces += 1;
            let line = format!(
                "debounce #{} armed for {delay:?} (the last one's task held: {})",
                slot.debounces,
                slot.debounce.is_some()
            );
            slot.trace.push(format!("{:.0} {line}", now_ms()));
            slot.debounces
        };
        // The repository is read once the events stop for a while, not once per event.
        slot.debounce = Some(cx.spawn(async move |workbench, cx| {
            cx.background_executor().timer(delay).await;
            // The window is gone: there is nothing to read for.
            // A read asked for while a Git command runs starts once the command ends.
            let _ = workbench.update(cx, |workbench, cx| {
                #[cfg(test)]
                workbench.vcs_trace(format!("debounce #{number} fired"));
                workbench.refresh_vcs(cx);
            });
        }));
    }

    /// Asks for the repository to be read again, without waiting for it.
    pub(super) fn refresh_vcs(&mut self, cx: &mut Context<Self>) {
        drop(self.request_vcs_refresh(cx));
    }

    /// Asks for the repository to be read again; the receiver is settled once a read that started
    /// after this call is shown.
    pub(super) fn request_vcs_refresh(&mut self, cx: &mut Context<Self>) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        let Some(slot) = self.vcs.as_mut() else { return receiver };
        let request = slot.queue.request(slot.state.available);
        #[cfg(test)]
        {
            let line = format!("read asked: {request:?}");
            slot.trace.push(format!("{:.0} {line}", now_ms()));
        }
        match request {
            RefreshRequest::Settled => {
                // The receiver may be gone: nobody waits.
                let _ = sender.send(());
            }
            RefreshRequest::Run { ticket, start } => {
                slot.waiters.push((ticket, sender));
                if start {
                    self.start_vcs_refresh(cx);
                }
            }
        }
        receiver
    }

    fn start_vcs_refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(slot) = self.vcs.as_mut().filter(|slot| slot.git_running.is_some()) {
            slot.read_deferred = true;
            #[cfg(test)]
            self.vcs_trace("read deferred: a Git command runs".to_owned());
            return;
        }
        #[cfg(test)]
        self.vcs_trace("read starts".to_owned());
        let label = self.label.clone();
        let Some(slot) = self.vcs.as_mut() else { return };
        let started = slot.state.begin_refresh();
        let repo = repo_path(started.repo());
        let read = host_call(label, cx, move |host, label| read_snapshot(host, label, repo));
        slot.refreshing = Some(cx.spawn(async move |workbench, cx| {
            let read = read.await;
            // The window is gone: nothing is shown any more.
            let _ = workbench.update(cx, |workbench, cx| workbench.vcs_refreshed(started, read, cx));
        }));
    }

    fn vcs_refreshed(&mut self, started: RefreshStart, read: Result<Snapshot, String>, cx: &mut Context<Self>) {
        #[cfg(test)]
        self.vcs_trace(match &read {
            Ok(snapshot) => format!("read done: {} files", snapshot.status.len()),
            Err(error) => format!("read failed: {error}"),
        });
        let Some(slot) = self.vcs.as_mut() else { return };
        let mut extras = None;
        let data = read.map(|snapshot| {
            extras = Some((snapshot.tags, snapshot.repo_branches));
            let status = snapshot.status.into_iter().map(crate::vcs::commit_view::file_status).collect();
            RefreshData { roots: snapshot.roots, status, selected: snapshot.selected }
        });
        let listed = data.as_ref().is_ok_and(|data| data.selected.is_ok());
        let outcome = slot.state.finish_refresh(&started, data);
        let conflicts = outcome.conflicts.clone();
        if listed && !outcome.again {
            slot.state.branches_loaded = true;
        }
        if let Some((tags, repo_branches)) = extras {
            slot.tags = tags;
            slot.repo_branches = repo_branches;
        }
        if let Some(error) = &slot.state.error {
            tracing::debug!("the repository could not be read: {error}");
        }
        let done = slot.queue.finished(slot.state.available);
        let (settled, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(&mut slot.waiters).into_iter().partition(|(ticket, _)| *ticket <= done.settled);
        slot.waiters = waiting;
        let again = done.start
            || (outcome.again
                && matches!(slot.queue.request(slot.state.available), RefreshRequest::Run { start: true, .. }));
        self.vcs_state_changed(cx);
        self.conflicts_changed(conflicts, cx);
        self.changelists_follow_status(cx);
        for (_, sender) in settled {
            // The waiter may be gone.
            let _ = sender.send(());
        }
        if again {
            self.start_vcs_refresh(cx);
        }
        self.wake_git_gate();
    }

    /// The command `number` started by [`git_command_starts`] ended: the read it held off starts,
    /// and who waits to run a command looks again. A command that ended already changes nothing.
    fn end_git_command(&mut self, number: u64, cx: &mut Context<Self>) {
        let Some(slot) = self.vcs.as_mut().filter(|slot| slot.git_running == Some(number)) else { return };
        slot.git_running = None;
        if std::mem::take(&mut slot.read_deferred) {
            self.start_vcs_refresh(cx);
        }
        self.wake_git_gate();
    }

    /// The operation marked `number` by [`mark_busy`] was dropped before it cleared its mark: the
    /// mark goes and the buttons that waited for it can be pressed again.
    fn clear_busy_mark(&mut self, number: u64, cx: &mut Context<Self>) {
        let Some(slot) = self.vcs.as_mut().filter(|slot| slot.busy_mark == Some(number)) else { return };
        clear_busy(slot);
        self.vcs_state_changed(cx);
    }

    fn wake_git_gate(&mut self) {
        let Some(slot) = self.vcs.as_mut() else { return };
        for waiter in slot.gate_waiters.drain(..) {
            // The waiter may be gone.
            let _ = waiter.send(());
        }
    }

    /// The widget and the open popup show the state again.
    fn vcs_state_changed(&mut self, cx: &mut Context<Self>) {
        let popup = self.branches_popup();
        let active = self.editor().map(|editor| editor.read(cx).path().to_string_lossy().into_owned());
        let Some(slot) = self.vcs.as_ref() else { return };
        let current = current_repo_for(active.as_deref(), &slot.state.roots, slot.state.repo.as_deref());
        let widget_state = BranchWidgetState::from_vcs(&slot.state, current, None, popup.is_some());
        slot.widget.update(cx, |widget, cx| widget.set_state(widget_state, cx));
        let merge_state = MergeRebaseState {
            operation: slot.state.branch.as_ref().and_then(|branch| branch.operation.clone()),
            conflicts: !slot.state.conflicted_files().is_empty(),
            busy: slot.state.remote_busy.is_some(),
        };
        slot.merge_widget.update(cx, |widget, cx| widget.set_state(merge_state, cx));
        if let Some(popup) = popup {
            let input = self.branches_input(cx);
            popup.update(cx, |popup, cx| popup.set_input(input, cx));
        }
        self.sync_commit_box(cx);
        self.sync_log_panes(cx);
        self.github_vcs_changed(cx);
        cx.notify();
    }

    /// What the Branches popup shows.
    fn branches_input(&self, cx: &App) -> BranchesPopupInput {
        let Some(slot) = self.vcs.as_ref() else { return BranchesPopupInput::default() };
        let state = &slot.state;
        let active = self.editor().map(|editor| editor.read(cx).path().to_string_lossy().into_owned());
        let repos = state
            .roots
            .iter()
            .map(|root| {
                let head = state.heads.get(root);
                RepoEntry {
                    root: root.clone(),
                    name: state.repo_name(root).to_owned(),
                    head: head.and_then(|head| head.head.clone()),
                    head_commit: head.and_then(|head| head.head_commit.clone()),
                }
            })
            .collect();
        BranchesPopupInput {
            repo: state.repo.clone(),
            branch: state.branch.clone(),
            branches: state.branches.clone(),
            loaded: state.branches_loaded,
            recent: slot.recent.clone(),
            favorites: slot.favorites.clone(),
            tags: slot.tags.clone(),
            conflicts: state.conflicted_files().len(),
            extension_actions: Vec::new(),
            options: slot.options.clone(),
            multi_expanded: slot.multi_expanded.clone(),
            repos,
            repo_branches: slot.repo_branches.clone(),
            operations: state.operations.clone(),
            current: current_repo_for(active.as_deref(), &state.roots, state.repo.as_deref()).map(str::to_owned),
        }
    }

    /// Publishes the context keys the repository's state sets.
    pub(super) fn publish_vcs_context(&self, window: &mut Window, cx: &mut App) {
        // Only an operation that can be continued or aborted counts; a bisect does not.
        let operation = self
            .vcs_state()
            .and_then(|state| in_progress(state.branch.as_ref()?.operation.as_deref()))
            .map_or("", InProgress::id);
        set_context(window, cx, "vcsOperation", operation);
    }

    /// Puts `name` first among the recent branches, and stores them.
    fn remember_branch(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(slot) = self.vcs.as_mut() else { return };
        slot.recent = push_recent(&slot.recent, name, RECENT_LIMIT);
        let key = recent_key(slot.state.root.as_deref());
        store_names(&key, &slot.recent, cx);
    }

    /// Selects repository `root` for the branch operations; the receiver is settled once it is read.
    fn select_vcs_repo(&mut self, root: &str, cx: &mut Context<Self>) -> Option<oneshot::Receiver<()>> {
        let slot = self.vcs.as_mut()?;
        slot.state.select_repo(root).then(|| self.request_vcs_refresh(cx))
    }

    // ---------------------------------------------------------------------------------------------
    // The Branches popup

    /// Branches…: opens the popup, or closes it when it is open.
    pub fn toggle_branches_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branches_popup().is_some() {
            self.close_popup(window, cx);
            return;
        }
        if self.vcs.as_ref().is_none_or(|slot| !slot.state.available) {
            return;
        }
        let input = self.branches_input(cx);
        let popup = cx.new(|cx| BranchesPopup::new(input, cx));
        let events = cx.subscribe_in(&popup, window, Self::branches_popup_event);
        self.open_popup(&popup, events, window, cx);
        self.vcs_state_changed(cx);
        // The popup shows what is listed now and what a fresh read finds.
        self.refresh_vcs(cx);
    }

    /// A popup closed: when it was the Branches popup, the widget is no longer pressed.
    pub(super) fn vcs_popup_closed(&mut self, view: &AnyView, cx: &mut Context<Self>) {
        if view.clone().downcast::<BranchesPopup>().is_err() {
            return;
        }
        if let Some(slot) = self.vcs.as_ref() {
            let state = BranchWidgetState { open: false, ..slot.widget.read(cx).state().clone() };
            slot.widget.update(cx, |widget, cx| widget.set_state(state, cx));
        }
    }

    fn branches_popup_event(
        &mut self,
        _: &Entity<BranchesPopup>,
        event: &BranchesPopupEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            BranchesPopupEvent::Dismissed => self.close_popup(window, cx),
            BranchesPopupEvent::RunAction { id, arg, repo, close } => {
                if close {
                    self.close_popup(window, cx);
                }
                let selected = repo.and_then(|root| self.select_vcs_repo(&root, cx));
                match selected {
                    None => run_action_later(id, arg, window, cx),
                    Some(read) => cx
                        .spawn_in(window, async move |_, cx| {
                            // The action acts on the repository once it is read.
                            let _ = read.await;
                            let _ = cx.update(|window, cx| run_action_later(id, arg, window, cx));
                        })
                        .detach(),
                }
            }
            BranchesPopupEvent::Command(command) => {
                self.close_popup(window, cx);
                self.run_branch_command(command, window, cx);
            }
            BranchesPopupEvent::ToggleFavorite { target, branch } => self.toggle_favorite(&target, &branch, cx),
            BranchesPopupEvent::OptionsChanged(options) => {
                cx.default_global::<UiStorage>().set(OPTIONS_KEY, stored_options(&options));
                if let Some(slot) = self.vcs.as_mut() {
                    slot.options = options;
                }
            }
            BranchesPopupEvent::MultiExpansionChanged(expanded) => {
                if let Some(slot) = self.vcs.as_mut() {
                    slot.multi_expanded = expanded;
                }
            }
        }
    }

    /// Adds `branch` to the favourites of the project, or removes it; the popup shows it at once.
    fn toggle_favorite(&mut self, target: &RepoTarget, branch: &Branch, cx: &mut Context<Self>) {
        let Some(slot) = self.vcs.as_mut() else { return };
        let branches = match target {
            RepoTarget::Repo(root) => slot.repo_branches.get(root).unwrap_or(&slot.state.branches),
            RepoTarget::Selected | RepoTarget::All(_) => &slot.state.branches,
        };
        let names = toggled_favorite(slot.favorites.as_deref(), branches, branch);
        store_names(&favorites_key(slot.state.root.as_deref()), &names, cx);
        slot.favorites = Some(names);
        self.vcs_state_changed(cx);
    }

    /// Carries out a row of a branch's submenu.
    pub(super) fn run_branch_command(&mut self, command: BranchesCommand, window: &mut Window, cx: &mut Context<Self>) {
        let BranchesCommand { target, command } = command;
        let repo = match target {
            RepoTarget::Selected => self.vcs_state().and_then(|state| state.repo.clone()),
            RepoTarget::Repo(root) => {
                // The operation reads the selected repository's branches: it is read first.
                let _ = self.select_vcs_repo(&root, cx);
                Some(root)
            }
            RepoTarget::All(roots) => {
                tracing::warn!("a branch command in {} repositories at once is not available", roots.len());
                return;
            }
        };
        match command {
            BranchCommand::Checkout(name) => cx
                .spawn_in(window, async move |this, cx| finished(checkout_branch(this, cx, name, repo).await))
                .detach(),
            BranchCommand::NewBranch { from } => self.start_new_branch(from, repo, window, cx),
            BranchCommand::Rename(name) => {
                cx.spawn_in(window, async move |this, cx| finished(rename(this, cx, name, repo).await)).detach()
            }
            BranchCommand::Delete(name) => {
                cx.spawn_in(window, async move |this, cx| finished(delete(this, cx, name, repo).await)).detach()
            }
            BranchCommand::DeleteRemote(name) => {
                cx.spawn_in(window, async move |this, cx| finished(delete_remote(this, cx, name, repo).await)).detach()
            }
            BranchCommand::CheckoutAndRebase(name) => cx
                .spawn_in(window, async move |this, cx| finished(checkout_and_rebase(this, cx, name, repo).await))
                .detach(),
            BranchCommand::RebaseOnto { branch, reference } => cx
                .spawn_in(window, async move |this, cx| {
                    finished(merge_or_rebase(&this, cx, branch, reference, repo, true).await)
                })
                .detach(),
            BranchCommand::Merge { branch, reference } => cx
                .spawn_in(window, async move |this, cx| {
                    finished(merge_or_rebase(&this, cx, branch, reference, repo, false).await)
                })
                .detach(),
            BranchCommand::PullInto { remote_branch, rebase } => cx
                .spawn_in(window, async move |this, cx| finished(pull(this, cx, remote_branch, rebase, repo).await))
                .detach(),
            BranchCommand::Update(name) => {
                cx.spawn_in(window, async move |this, cx| finished(update_branch(this, cx, name, repo).await)).detach()
            }
            BranchCommand::ToggleFavorite(name) => {
                let branch = self.vcs_state().and_then(|state| state.branches.iter().find(|b| b.name == name).cloned());
                if let Some(branch) = branch {
                    self.toggle_favorite(&RepoTarget::Selected, &branch, cx);
                }
            }
            BranchCommand::Action { id, arg } => run_action_later(id.to_owned(), arg, window, cx),
            BranchCommand::Compare(name) | BranchCommand::ShowDiffWithWorkingTree(name) => {
                tracing::info!("comparing {name} has no window to show it in");
            }
            // The Git log's Branches pane deletes its selection one branch after another; a tag
            // has no Delete here.
            BranchCommand::DeleteSelection(targets) => cx
                .spawn_in(window, async move |this, cx| {
                    for target in targets {
                        let repo = target.repo.clone().or_else(|| repo.clone());
                        let deleted = match target.branch {
                            Some(branch) if branch.remote => delete_remote(this.clone(), cx, branch.name, repo).await,
                            Some(branch) => delete(this.clone(), cx, branch.name, repo).await,
                            None => {
                                tracing::info!("{} is not a branch: it is not deleted", target.ref_name);
                                Ok(())
                            }
                        };
                        if deleted.is_err() {
                            return finished(deleted);
                        }
                    }
                })
                .detach(),
        }
    }

    // ---------------------------------------------------------------------------------------------
    // The operations the actions start

    fn start_new_branch(
        &mut self,
        from: Option<String>,
        repo: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo = repo.or_else(|| self.vcs_state().and_then(|state| state.repo.clone()));
        cx.spawn_in(window, async move |this, cx| finished(new_branch(this, cx, from, repo).await)).detach();
    }

    fn start_checkout_revision(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| finished(checkout_revision(this, cx).await)).detach();
    }

    fn start_fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| finished(fetch(this, cx).await)).detach();
    }

    /// Push…: of the current branch. Another branch than the current one cannot be pushed from here.
    fn start_push(
        &mut self,
        branch: Option<String>,
        revision: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.vcs_state().and_then(|state| state.branch.as_ref()).and_then(|info| info.head.clone());
        if let Some(branch) = branch.filter(|branch| head.as_ref() != Some(branch)) {
            tracing::warn!("pushing branch {branch}, which is not checked out, is not available");
            return;
        }
        cx.spawn_in(window, async move |this, cx| finished(push(this, cx, revision).await)).detach();
    }

    /// A button of a notification raised here: whether it was one.
    pub(super) fn run_vcs_follow_up(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(slot) = self.vcs.as_mut() else { return false };
        let Some(at) = slot.follow_ups.iter().position(|(own, _)| own == command) else { return false };
        let follow_up = slot.follow_ups[at].1.clone();
        // The buttons' commands are `<notification key>:<button>`.
        if let Some((key, _)) = command.rsplit_once(':').filter(|_| follow_up.expires_notice()) {
            let prefix = format!("{key}:");
            slot.follow_ups.retain(|(own, _)| !own.starts_with(&prefix));
            self.notifications.expire_notification(key);
        }
        cx.spawn_in(window, async move |this, cx| finished(run_follow_up(this, cx, follow_up).await)).detach();
        true
    }

    // ---------------------------------------------------------------------------------------------
    // The dialogs

    /// Shows a dialog of an operation in place of any other; the receiver gets its answer.
    pub(super) fn show_prompt_dialog(
        &mut self,
        build: impl FnOnce(&mut Window, &mut Context<PromptDialog>) -> PromptDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<DialogAnswer> {
        let (sender, answer) = oneshot::channel();
        let dialog = cx.new(|cx| build(window, cx));
        let mut sender = Some(sender);
        let events = cx.subscribe_in(&dialog, window, move |workbench, _, event: &PromptDialogEvent, window, cx| {
            let PromptDialogEvent::Closed(closed) = event;
            workbench.close_vcs_dialog(window, cx);
            if let Some(sender) = sender.take() {
                // The operation may have ended meanwhile.
                let _ = sender.send(closed.clone());
            }
        });
        let focus = dialog.focus_handle(cx);
        self.open_vcs_dialog(dialog.into(), focus, events, window, cx);
        answer
    }

    /// Shows the Push dialog; the receiver gets the pushes asked for, `None` when it was cancelled.
    fn show_push_dialog(
        &mut self,
        input: PushDialogInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Option<Vec<PushRequest>>> {
        let (sender, answer) = oneshot::channel();
        let dialog = cx.new(|cx| PushDialog::new(input, window, cx));
        let mut sender = Some(sender);
        let events = cx.subscribe_in(&dialog, window, move |workbench, _, event: &PushDialogEvent, window, cx| {
            workbench.close_vcs_dialog(window, cx);
            let requests = match event {
                PushDialogEvent::Push(requests) => Some(requests.clone()),
                PushDialogEvent::Cancelled => None,
            };
            if let Some(sender) = sender.take() {
                // The operation may have ended meanwhile.
                let _ = sender.send(requests);
            }
        });
        let focus = dialog.focus_handle(cx);
        self.open_vcs_dialog(dialog.into(), focus, events, window, cx);
        answer
    }

    pub(super) fn open_vcs_dialog(
        &mut self,
        view: AnyView,
        focus: FocusHandle,
        events: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Another dialog in its place gives the keyboard back to where it was before the first.
        let restore = match self.vcs_dialog.take() {
            Some(previous) => previous.restore,
            None => window.focused(cx),
        };
        window.focus(&focus, cx);
        self.vcs_dialog = Some(VcsDialog { view, restore, _events: events });
        cx.notify();
    }

    pub(super) fn close_vcs_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.vcs_dialog.take() else { return };
        if let Some(target) = dialog.restore.or_else(|| self.editor().map(|editor| editor.focus_handle(cx))) {
            window.focus(&target, cx);
        }
        cx.notify();
    }

    /// The dialog of a Git operation over the window, while one is open.
    pub(super) fn vcs_dialog_layer(&self) -> Option<AnyElement> {
        let view = self.vcs_dialog.as_ref()?.view.clone();
        let layer = div()
            .id("vcs-dialog-layer")
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

    // Not from a spec: the gear options are stored as the three flags; anything else keeps the
    // defaults.
    #[test]
    fn stores_and_reads_the_popup_options() {
        let options = BranchTreeOptions { group_by_prefix: false, show_tags: false, ..BranchTreeOptions::default() };
        let stored = stored_options(&options);
        let value: Value = serde_json::from_str(&stored).expect("the options are JSON");
        assert_eq!(value, serde_json::json!({ "groupByPrefix": false, "showRecent": true, "showTags": false }));
        let read = load_options(Some(&stored));
        assert_eq!((read.group_by_prefix, read.show_recent, read.show_tags), (false, true, false));
        assert!(read.expanded.is_empty(), "the groups opened are not stored");
        let defaults = BranchTreeOptions::default();
        for broken in [None, Some("null"), Some("[1]"), Some("{\"showRecent\": \"no\"}"), Some("{")] {
            let read = load_options(broken);
            assert_eq!(
                (read.group_by_prefix, read.show_recent, read.show_tags),
                (defaults.group_by_prefix, defaults.show_recent, defaults.show_tags),
                "{broken:?}"
            );
        }
    }
}
