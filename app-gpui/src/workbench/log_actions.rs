//! The actions of the Git log: the commit context menu and toolbar (cherry-pick, revert, reset,
//! checkout, comparisons, history rewriting, new branch and tag, navigation, View in Browser),
//! the Branches pane's toolbar and the changed files pane's toolbar and menu.
//!
//! A commit action acts on the commits its argument names (a commit key, or a list of them, as the
//! commit menu passes), else on the log's selection; only commits the log has read count, newest
//! first as the log lists them. Without one it says to select a commit. A Branches pane action acts
//! on its argument (a ref with its repository and branch), else on the pane's selection; a changed
//! files action on the files its argument names (repository-relative paths), else on the pane's
//! selection.
//!
//! Every Git operation runs on the backend, logs to the Git console, reads the repository again and
//! then reports what it did. An operation that may stop on conflicts opens them for resolving
//! instead of failing. A history rewrite offers Undo, which puts HEAD back while it is still where
//! the rewrite left it.

use super::branches::{
    GitRun, OPERATION_KEY, Success, This, call, checkout, finished, notify, refresh, run_action_later,
    run_git_operation, run_op, save_all, vcs_state,
};
use super::conflicts::{RESOLVE_CONFLICTS, save_exact};
use super::services_window::on_workbench;
use super::shelf::PatchMade;
use super::vcs_operations::{ask_form, reload_log};
use super::{GIT_TOOL_WINDOW, Workbench};
use crate::agents::conversation::decode_uri_component;
use crate::platform::actions::ActionRegistry;
use crate::platform::i18n::{I18n, Localization, Param};
use crate::platform::settings;
use crate::platform::storage::UiStorage;
use crate::vcs::branches_popup::{BranchesCommand, RepoTarget};
use crate::vcs::changes::base_name;
use crate::vcs::git_forms::{FormAnswer, ResetForm, TagForm, reset_mode_name};
use crate::vcs::interactive_rebase_dialog::{InteractiveRebaseDialog, InteractiveRebaseEvent};
use crate::vcs::log_details::{ChangesCommand, filter_patch};
use crate::vcs::log_message_dialog::{MessageDialog, MessageDialogEvent};
use crate::vcs::log_options::{LogOptions, STORAGE_KEY, parse_log_options};
use crate::vcs::patch_dialogs::CreatePatchSource;
use crate::vcs::prompt_dialogs::{FieldRule, OK, PromptDialog};
use crate::vcs_diff::diff_input::{DiffData, DiffSide, repository_path, with_chain};
use crate::vcs_diff::model::commit_web_url;
use crate::vcs_more::log_changes::LogChange;
use crate::vcs_more::new_tag::NewTagOn;
use crate::vcs_ops::branch_menu::BranchCommand;
use crate::vcs_ops::branch_ops::can_delete_branch;
use crate::vcs_ops::dialogs::{AskAnswer, AskButton, AskRequest, ConfirmRequest, PromptRequest};
use crate::vcs_ops::git_log::{CommitMenuContext, LogCommand, commit_menu};
use crate::vcs_ops::log_pane_actions::{
    BranchTarget, branch_arg, branch_targets, can_update_branch, new_branch_source,
};
use crate::vcs_ops::log_store::{GitLog as LogModel, RepoCommit};
use crate::vcs_ops::menu::MenuItem;
use crate::vcs_ops::operations::{InProgress, in_progress};
use crate::vcs_ops::ops::vcs_notice;
use crate::vcs_ops::patches::{PatchOrigin, suggested_patch_name};
use crate::vcs_ops::rebase_plan::{
    RebaseRange, head_pushed, initial_rows, rebase_range, single_commit_plan, squash_plan, to_entries,
    undo_commit_problem,
};
use crate::vcs_ops::remote_ui::is_protected_branch;
use crate::workbench::notifications::{NotificationAction, NotificationLevel};
use futures::channel::oneshot;
use gpui_kit::{
    App, AppContext, AsyncWindowContext, ClipboardItem, Context, Entity, EventEmitter, Focusable, Pixels, Point,
    Render, Window, px,
};
use ide_core::IdeError;
use ide_host::Host;
use ide_host::ops::vcs::{self as git, GitOperation, PatchSource};
use ide_vcs::{ChangeKind, ChangedFile, Commit, LogQuery, RebaseAction, RebaseEntry};
use ide_workspace::settings::SettingsScope;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Commands of the Undo buttons of history rewrites: `vcs.log.undo:<notification key>`.
pub(super) const UNDO_PREFIX: &str = "vcs.log.undo:";

/// What the Git log's actions keep between an operation and its notification's buttons.
#[derive(Default)]
pub(super) struct LogActionsSlot {
    /// What each Undo button puts back, by its command.
    undo: Vec<(String, UndoHead)>,
}

/// HEAD as a history rewrite found it and left it: Undo goes back while HEAD is still `after`.
#[derive(Clone, Debug)]
struct UndoHead {
    repo: Option<String>,
    before: String,
    after: String,
    /// What the notification says once undone.
    undone: String,
    /// The key of the notification that offers it.
    key: String,
}

/// A commit action of the log, run on the commits it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitAction {
    CopyRevision,
    CreatePatch,
    CherryPick,
    CheckoutRevision,
    CompareWithLocal,
    CompareVersions,
    ResetToHere,
    Revert,
    UndoCommit,
    EditMessage,
    DropCommit,
    SquashCommits,
    Fixup,
    SquashInto,
    PushUpToHere,
    RebaseOntoCommit,
    GoToChild,
    GoToParent,
    OpenInBrowser,
    InteractiveRebase,
    NewBranch,
    NewTag,
}

impl CommitAction {
    /// The action acts on every given commit, not only the first.
    fn many(self) -> bool {
        matches!(
            self,
            Self::CopyRevision
                | Self::CreatePatch
                | Self::CherryPick
                | Self::CompareVersions
                | Self::Revert
                | Self::DropCommit
                | Self::SquashCommits
        )
    }
}

const COMMIT_ACTIONS: [(&str, CommitAction); 22] = [
    ("vcs.log.copyRevision", CommitAction::CopyRevision),
    ("vcs.log.createPatch", CommitAction::CreatePatch),
    ("vcs.log.cherryPick", CommitAction::CherryPick),
    ("vcs.log.checkoutRevision", CommitAction::CheckoutRevision),
    ("vcs.log.compareWithLocal", CommitAction::CompareWithLocal),
    ("vcs.log.compareVersions", CommitAction::CompareVersions),
    ("vcs.log.resetToHere", CommitAction::ResetToHere),
    ("vcs.log.revert", CommitAction::Revert),
    ("vcs.log.undoCommit", CommitAction::UndoCommit),
    ("vcs.log.editMessage", CommitAction::EditMessage),
    ("vcs.log.dropCommit", CommitAction::DropCommit),
    ("vcs.log.squashCommits", CommitAction::SquashCommits),
    ("vcs.log.fixup", CommitAction::Fixup),
    ("vcs.log.squashInto", CommitAction::SquashInto),
    ("vcs.log.pushUpToHere", CommitAction::PushUpToHere),
    ("vcs.log.rebaseOntoCommit", CommitAction::RebaseOntoCommit),
    ("vcs.log.goToChild", CommitAction::GoToChild),
    ("vcs.log.goToParent", CommitAction::GoToParent),
    ("vcs.log.openInBrowser", CommitAction::OpenInBrowser),
    ("vcs.log.interactiveRebase", CommitAction::InteractiveRebase),
    ("vcs.log.newBranch", CommitAction::NewBranch),
    ("vcs.log.newTag", CommitAction::NewTag),
];

/// An action of the Branches pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneAction {
    New,
    Update,
    Delete,
    ToggleFavorite,
    Navigate,
}

const PANE_ACTIONS: [(&str, PaneAction); 5] = [
    ("vcs.log.branches.new", PaneAction::New),
    ("vcs.log.branches.update", PaneAction::Update),
    ("vcs.log.branches.delete", PaneAction::Delete),
    ("vcs.log.branches.toggleFavorite", PaneAction::ToggleFavorite),
    ("vcs.log.branches.navigate", PaneAction::Navigate),
];

/// The changed files actions that are rows of the pane's menu.
const CHANGES_ACTIONS: [(&str, ChangesCommand); 6] = [
    ("vcs.log.changes.showDiff", ChangesCommand::ShowDiff),
    ("vcs.log.changes.compareWithLocal", ChangesCommand::CompareWithLocal),
    ("vcs.log.changes.compareBeforeWithLocal", ChangesCommand::CompareBeforeWithLocal),
    ("vcs.log.changes.jumpToSource", ChangesCommand::JumpToSource),
    ("vcs.log.changes.getFromRevision", ChangesCommand::GetFromRevision),
    ("vcs.log.changes.revert", ChangesCommand::Revert),
];

const GO_TO_REF: &str = "vcs.log.goToRef";
const REVEAL_BOOKMARK: &str = "vcs.log.revealBookmark";
const COLLAPSE_LINEAR: &str = "vcs.log.collapseLinear";
const EXPAND_LINEAR: &str = "vcs.log.expandLinear";
const GROUP_BY_DIRECTORY: &str = "vcs.log.changes.groupByDirectory";
const DIFF_PREVIEW: &str = "vcs.log.changes.diffPreview";

/// The log's options as stored.
fn stored_log_options(cx: &App) -> LogOptions {
    let stored = cx.try_global::<UiStorage>().and_then(|storage| storage.get(STORAGE_KEY));
    parse_log_options(stored.and_then(|text| serde_json::from_str::<Value>(text).ok()).as_ref())
}

pub(super) fn register_actions(registry: &mut ActionRegistry) {
    for (id, action) in COMMIT_ACTIONS {
        registry.handle(id, move |call, window, cx| {
            let arg = call.arg.cloned();
            on_workbench(window, cx, move |workbench, window, cx| {
                workbench.run_commit_action(action, arg.as_ref(), window, cx)
            })
        });
    }
    for (id, action) in PANE_ACTIONS {
        registry.handle(id, move |call, window, cx| {
            let arg = call.arg.cloned();
            on_workbench(window, cx, move |workbench, window, cx| {
                workbench.run_pane_action(action, arg.as_ref(), window, cx)
            })
        });
    }
    for (id, command) in CHANGES_ACTIONS {
        registry.handle(id, move |call, window, cx| {
            let arg = call.arg.cloned();
            let command = command.clone();
            on_workbench(window, cx, move |workbench, window, cx| {
                let changes = workbench.log_changes_arg(arg.as_ref(), cx);
                workbench.run_log_changes_action(command, changes, window, cx)
            })
        });
    }
    registry.handle(GO_TO_REF, |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.start_go_to_ref(window, cx))
    });
    registry.handle(REVEAL_BOOKMARK, |call, window, cx| {
        let arg = call.arg.and_then(Value::as_str).map(str::to_owned);
        on_workbench(window, cx, move |workbench, window, cx| workbench.reveal_bookmark(arg.as_deref(), window, cx))
    });
    for (id, collapse) in [(COLLAPSE_LINEAR, true), (EXPAND_LINEAR, false)] {
        registry.handle(id, move |_, window, cx| {
            on_workbench(window, cx, move |workbench, _, cx| {
                workbench.update_log_options(|options| options.collapse_linear = collapse, cx)
            })
        });
    }
    registry.handle_toggle(
        GROUP_BY_DIRECTORY,
        |_, cx| stored_log_options(cx).group_by_directory,
        |_, window, cx| {
            on_workbench(window, cx, |workbench, _, cx| {
                workbench.update_log_options(|options| options.group_by_directory = !options.group_by_directory, cx)
            })
        },
    );
    registry.handle_toggle(
        DIFF_PREVIEW,
        |_, cx| stored_log_options(cx).diff_preview,
        |_, window, cx| {
            on_workbench(window, cx, |workbench, _, cx| {
                workbench.update_log_options(|options| options.diff_preview = !options.diff_preview, cx)
            })
        },
    );
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn repo_path(repo: Option<&str>) -> Option<PathBuf> {
    repo.filter(|repo| !repo.is_empty()).map(PathBuf::from)
}

/// The commit of `key`, a commit key of the log (`<repository>\0<id>` in a log of several).
fn key_id(key: &str) -> &str {
    key.rsplit_once('\0').map_or(key, |(_, id)| id)
}

/// The commit ids an action's argument names: a key, or a list of keys.
fn arg_commit_ids(arg: Option<&Value>) -> Vec<String> {
    match arg {
        Some(Value::Array(keys)) => keys.iter().filter_map(Value::as_str).map(|key| key_id(key).to_owned()).collect(),
        Some(Value::String(key)) => vec![key_id(key).to_owned()],
        _ => Vec::new(),
    }
}

/// The commits `ids` of `repo` as the backend reads them, in the order given; those it cannot read
/// are left out.
async fn read_commits(
    this: &This,
    cx: &mut AsyncWindowContext,
    repo: Option<String>,
    ids: Vec<String>,
) -> anyhow::Result<Vec<Commit>> {
    let path = repo_path(repo.as_deref());
    let read = call(this, cx, move |host, label| async move {
        let mut commits = Vec::new();
        for id in ids {
            let query = LogQuery { branch: Some(id), limit: 1, ..LogQuery::default() };
            match git::vcs_log(&host, &label, path.clone(), query).await {
                Ok(found) => commits.extend(found.into_iter().next()),
                Err(error) => tracing::debug!("a commit of the action cannot be read: {}", error.message),
            }
        }
        Ok(commits)
    })
    .await?;
    Ok(read.unwrap_or_default())
}

/// `f` with the UI language.
fn texts<R>(this: &This, cx: &mut AsyncWindowContext, f: impl FnOnce(&mut I18n) -> R) -> anyhow::Result<R> {
    this.update(cx, |_, cx| f(&mut cx.default_global::<Localization>().0))
}

/// What a message names the commits by: one by its short hash and subject, several by their number.
fn commits_label(commits: &[Commit], i18n: &mut I18n) -> String {
    match commits {
        [one] => format!("{} “{}”", short(&one.id), one.summary),
        many => i18n.tx_built(&["", " commits"], &[Param::from(many.len())]),
    }
}

/// The question before rewriting a commit that is already on the upstream.
fn pushed_warning(upstream: Option<&str>, what: &str) -> String {
    let upstream = upstream.filter(|upstream| !upstream.is_empty()).unwrap_or("the remote");
    format!(
        "The commit is already pushed to {upstream}. {what} rewrites published history; {}",
        "pushing afterwards needs a force push."
    )
}

/// `verb` with its first letter in upper case: the title of its question.
fn capitalized(verb: &str) -> String {
    let mut chars = verb.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

// -------------------------------------------------------------------------------------------------
// Dialogs

/// Shows the dialog `build` makes and waits for its answer; a dialog dropped without one answers
/// Cancel.
async fn ask(this: &This, cx: &mut AsyncWindowContext, request: AskRequest) -> anyhow::Result<AskAnswer> {
    let answer = this.update_in(cx, move |workbench, window, cx| {
        workbench.show_prompt_dialog(move |window, cx| PromptDialog::ask(request, window, cx), window, cx)
    })?;
    Ok(answer.await.map(|answer| answer.ask_answer()).unwrap_or_default())
}

async fn confirm(this: &This, cx: &mut AsyncWindowContext, request: ConfirmRequest) -> anyhow::Result<bool> {
    let answer = this.update_in(cx, move |workbench, window, cx| {
        workbench.show_prompt_dialog(move |window, cx| PromptDialog::confirm(request, window, cx), window, cx)
    })?;
    Ok(answer.await.is_ok_and(|answer| answer.confirmed()))
}

/// A one-line prompt; `None` when it was cancelled.
async fn prompt(
    this: &This,
    cx: &mut AsyncWindowContext,
    request: PromptRequest,
    rule: FieldRule,
) -> anyhow::Result<Option<String>> {
    let answer = this.update_in(cx, move |workbench, window, cx| {
        workbench.show_prompt_dialog(move |window, cx| PromptDialog::prompt(request, rule, window, cx), window, cx)
    })?;
    Ok(answer.await.ok().and_then(|answer| answer.prompt_result()).map(|result| result.value))
}

/// A refused history rewrite is reported in an error dialog, as IDEA does.
async fn refuse(this: &This, cx: &mut AsyncWindowContext, title: String, body: String) -> anyhow::Result<()> {
    let request = AskRequest {
        title,
        body,
        warning: true,
        buttons: vec![AskButton { id: OK, label: "OK", primary: true }],
        ..AskRequest::default()
    };
    ask(this, cx, request).await?;
    Ok(())
}

/// The commit message dialog titled `title`; the message, `None` when it was cancelled.
async fn message_dialog(
    this: &This,
    cx: &mut AsyncWindowContext,
    title: String,
    initial: String,
) -> anyhow::Result<Option<String>> {
    let answer = this.update_in(cx, move |workbench, window, cx| {
        let dialog = cx.new(|cx| MessageDialog::new(&title, &initial, cx));
        workbench.open_answer_dialog(
            dialog,
            |event: &MessageDialogEvent| {
                let MessageDialogEvent::Closed(message) = event;
                message.clone()
            },
            window,
            cx,
        )
    })?;
    Ok(answer.await.ok().flatten())
}

// -------------------------------------------------------------------------------------------------
// Running Git operations

/// The commit HEAD of `repo` is on.
async fn head_commit(this: &This, cx: &mut AsyncWindowContext, repo: Option<String>) -> anyhow::Result<Option<String>> {
    let path = repo_path(repo.as_deref());
    let info =
        call(this, cx, move |host, label| async move { git::vcs_branch_info(&host, &label, path).await }).await?;
    Ok(info.ok().and_then(|info| info.head_commit).filter(|id| !id.is_empty()))
}

/// Rewrites the branch from `base` with `entries`; a finished rewrite can be undone from its
/// notification, which then says `undone`.
async fn run_rebase(
    this: &This,
    cx: &mut AsyncWindowContext,
    base: Option<String>,
    entries: Vec<RebaseEntry>,
    done: String,
    undone: &str,
) -> anyhow::Result<()> {
    let (failure, stopped, undone) = texts(this, cx, |i18n| {
        (
            i18n.tx("Rebase failed"),
            i18n.tx("Rebase stopped for editing; continue the rebase when done"),
            i18n.tx(undone),
        )
    })?;
    let repo = vcs_state(this, cx, |state| state.repo.clone())?;
    let before = head_commit(this, cx, repo.clone()).await?;
    let run = GitRun {
        op: GitOperation::InteractiveRebase { base, entries },
        progress: "Rebasing",
        success: done.clone(),
        stopped: Some(stopped),
        failure,
        conflicts_instead: true,
    };
    let ok = run_git_operation(this, cx, run).await?;
    let rebasing = vcs_state(this, cx, |state| {
        in_progress(state.branch.as_ref().and_then(|branch| branch.operation.as_deref())) == Some(InProgress::Rebase)
    })?;
    if ok
        && !rebasing
        && let (Some(before), Some(after)) = (before, head_commit(this, cx, repo.clone()).await?)
    {
        let head = UndoHead { repo, before, after, undone, key: OPERATION_KEY.to_owned() };
        this.update(cx, |workbench, cx| {
            let actions = workbench.undo_link(head);
            // The success notice is told again, now with its Undo button.
            if !actions.is_empty() {
                let mut notice = vcs_notice(NotificationLevel::Info, done);
                notice.key = Some(OPERATION_KEY.to_owned());
                notice.actions = actions;
                workbench.notify(notice, cx);
            }
        })?;
    }
    reload_log(this, cx)
}

// -------------------------------------------------------------------------------------------------
// Commit actions

async fn cherry_pick(this: This, cx: &mut AsyncWindowContext, commits: Vec<Commit>) -> anyhow::Result<()> {
    // Oldest first, as IDEA applies them.
    let ordered: Vec<Commit> = commits.into_iter().rev().collect();
    let (repo, remotes, protected) = this.update(cx, |workbench, cx| {
        let state = workbench.vcs_state();
        let remotes: HashSet<String> = state
            .map(|state| {
                state.branches.iter().filter(|branch| branch.remote).map(|branch| branch.name.clone()).collect()
            })
            .unwrap_or_default();
        let protected = settings::effective(&workbench.label, cx).vcs.git.protected_branches;
        (state.and_then(|state| state.repo.clone()), remotes, protected)
    })?;
    // Commits already pushed to a protected branch get "(cherry picked from commit …)".
    let ids: Vec<String> = ordered.iter().map(|commit| commit.id.clone()).collect();
    let path = repo_path(repo.as_deref());
    let read = ids.clone();
    let containing = call(&this, cx, move |host, label| async move {
        let mut out = Vec::with_capacity(read.len());
        for id in read {
            // A commit whose details cannot be read counts as not pushed.
            out.push(git::vcs_log_details(&host, &label, path.clone(), id).await.ok().map(|d| d.containing_branches));
        }
        Ok(out)
    })
    .await?
    .unwrap_or_default();
    let from_protected = |branches: &Option<Vec<String>>| {
        branches.as_ref().is_some_and(|branches| {
            branches.iter().any(|branch| {
                let name = branch.split_once('/').map_or(branch.as_str(), |(_, name)| name);
                remotes.contains(branch) && is_protected_branch(name, &protected)
            })
        })
    };
    let record_origin = containing.len() == ids.len() && containing.iter().all(from_protected);
    let (success, failure) = texts(&this, cx, |i18n| {
        let label = commits_label(&ordered, i18n);
        (i18n.tx_built(&["Cherry-picked ", ""], &[Param::from(label.as_str())]), i18n.tx("Cherry-pick failed"))
    })?;
    let run = GitRun {
        op: GitOperation::CherryPick { commits: ids, record_origin },
        progress: "Cherry-picking",
        success,
        stopped: None,
        failure,
        conflicts_instead: true,
    };
    run_git_operation(&this, cx, run).await?;
    reload_log(&this, cx)
}

async fn revert(this: This, cx: &mut AsyncWindowContext, commits: Vec<Commit>) -> anyhow::Result<()> {
    let (success, failure) = texts(&this, cx, |i18n| {
        let label = commits_label(&commits, i18n);
        (i18n.tx_built(&["Reverted ", ""], &[Param::from(label.as_str())]), i18n.tx("Revert failed"))
    })?;
    let run = GitRun {
        // Newest first.
        op: GitOperation::Revert { commits: commits.iter().map(|commit| commit.id.clone()).collect() },
        progress: "Reverting",
        success,
        stopped: None,
        failure,
        conflicts_instead: true,
    };
    run_git_operation(&this, cx, run).await?;
    reload_log(&this, cx)
}

/// Reset Current Branch to Here…: the Git Reset dialog on the commit, then `git reset` in the chosen
/// mode.
async fn reset_to(this: This, cx: &mut AsyncWindowContext, commit: Commit) -> anyhow::Result<()> {
    let id = short(&commit.id);
    let form = ResetForm::on_commit(&commit.id, &id, &commit.summary);
    let Some(FormAnswer::Reset { commit: target, mode }) = ask_form(&this, cx, Box::new(form)).await? else {
        return Ok(());
    };
    let head = vcs_state(&this, cx, |state| state.branch.as_ref().and_then(|branch| branch.head.clone()))?;
    let (success, failure) = texts(&this, cx, |i18n| {
        let head = head.as_deref().filter(|head| !head.is_empty()).unwrap_or("HEAD");
        let values = [Param::from(head), Param::from(id.as_str()), Param::from(reset_mode_name(mode))];
        (i18n.tx_built(&["Reset ", " to ", " (", ")"], &values), i18n.tx("Reset failed"))
    })?;
    let run = GitRun {
        op: GitOperation::Reset { commit: target, mode },
        progress: "Resetting",
        success,
        stopped: None,
        failure,
        conflicts_instead: false,
    };
    run_git_operation(&this, cx, run).await?;
    reload_log(&this, cx)
}

/// The diff window on the first of `files`, with the others to move along; `left` and `right` make
/// the sides of a file.
fn compared(
    files: &[ChangedFile],
    root: Option<&str>,
    left: impl Fn(&ChangedFile) -> DiffSide,
    right: impl Fn(&ChangedFile) -> DiffSide,
) -> Option<DiffData> {
    let items: Vec<DiffData> =
        files.iter().map(|file| DiffData::new(&repository_path(root, &file.path), left(file), right(file))).collect();
    with_chain(&items, 0)
}

/// A revision side of a comparison: missing for a file the change added.
fn revision_side(file: &ChangedFile, rev: &str, root: Option<&str>) -> DiffSide {
    match file.kind {
        ChangeKind::Added | ChangeKind::Untracked => DiffSide::text("", "(does not exist)"),
        _ => DiffSide::Rev {
            rev: rev.to_owned(),
            path: file.old_path.as_deref().filter(|old| !old.is_empty()).map(|old| repository_path(root, old)),
            label: Some(short(rev)),
        },
    }
}

/// Compare with Local (`newer` absent) and Compare Versions: the files that differ between `older`
/// and the working tree (or `newer`), in the diff window.
async fn compare(this: This, cx: &mut AsyncWindowContext, older: Commit, newer: Option<Commit>) -> anyhow::Result<()> {
    let (repo, root) =
        vcs_state(&this, cx, |state| (state.repo.clone(), state.current_repo_root().map(str::to_owned)))?;
    let path = repo_path(repo.as_deref());
    let (from, to) = (older.id.clone(), newer.as_ref().map(|commit| commit.id.clone()));
    let read = call(&this, cx, move |host, label| async move {
        match to {
            Some(to) => git::vcs_diff_revisions(&host, &label, path, from, to).await,
            None => git::vcs_diff_with_worktree(&host, &label, path, from).await,
        }
    })
    .await?;
    let files = match read {
        Ok(files) => files,
        Err(error) => {
            let mut notice = vcs_notice(NotificationLevel::Error, "Could not open comparison");
            notice.body = Some(error);
            return notify(&this, cx, notice);
        }
    };
    let root = root.as_deref();
    let right = |file: &ChangedFile| match (&newer, file.kind) {
        (_, ChangeKind::Deleted) => DiffSide::text("", "(deleted)"),
        (Some(newer), _) => DiffSide::labelled_rev(&newer.id, &short(&newer.id)),
        (None, _) => DiffSide::worktree(),
    };
    match compared(&files, root, |file| revision_side(file, &older.id, root), right) {
        Some(data) => this.update_in(cx, |workbench, window, cx| workbench.open_diff(data, window, cx)),
        None => {
            let title = texts(&this, cx, |i18n| {
                i18n.tx_built(&["No changes against ", " in 1 repository"], &[Param::from(short(&older.id).as_str())])
            })?;
            notify(&this, cx, vcs_notice(NotificationLevel::Info, title))
        }
    }
}

/// The commits from `target` to HEAD, when they can be rewritten; otherwise what stands in the way
/// is reported and `None` comes back. A pushed HEAD asks first.
async fn range_from(
    this: &This,
    cx: &mut AsyncWindowContext,
    target: &Commit,
    verb: &str,
) -> anyhow::Result<Option<(RebaseRange, Option<String>)>> {
    refresh(this, cx).await?;
    let (repo, branch) = vcs_state(this, cx, |state| (state.repo.clone(), state.branch.clone()))?;
    if let Some(op) = in_progress(branch.as_ref().and_then(|branch| branch.operation.as_deref())) {
        refuse(this, cx, format!("Cannot {verb}"), format!("A {} is in progress", op.id())).await?;
        return Ok(None);
    }
    let query = LogQuery {
        branch: Some("HEAD".to_owned()),
        range: target.parents.first().map(|parent| format!("{parent}..HEAD")),
        limit: 0,
        ..LogQuery::default()
    };
    let path = repo_path(repo.as_deref());
    let history =
        call(this, cx, move |host, label| async move { git::vcs_log(&host, &label, path, query).await }).await?;
    let range = match history.map(|history| rebase_range(target, &history).map_err(str::to_owned)) {
        Ok(Ok(range)) => range,
        Ok(Err(problem)) | Err(problem) => {
            refuse(this, cx, format!("Cannot {verb}"), problem).await?;
            return Ok(None);
        }
    };
    if head_pushed(branch.as_ref()) {
        let title = capitalized(verb);
        let upstream = branch.as_ref().and_then(|branch| branch.upstream.as_deref());
        let request = ConfirmRequest::danger(&title, pushed_warning(upstream, &title), "Continue");
        if !confirm(this, cx, request).await? {
            return Ok(None);
        }
    }
    Ok(Some((range, repo)))
}

/// The range from the oldest of `commits` (newest first), when all of them are in it.
async fn range_for(
    this: &This,
    cx: &mut AsyncWindowContext,
    commits: &[Commit],
    verb: &str,
) -> anyhow::Result<Option<(RebaseRange, Option<String>)>> {
    let Some(oldest) = commits.last() else { return Ok(None) };
    let Some((range, repo)) = range_from(this, cx, oldest, verb).await? else { return Ok(None) };
    let listed: HashSet<&str> = range.commits.iter().map(|commit| commit.id.as_str()).collect();
    if !commits.iter().all(|commit| listed.contains(commit.id.as_str())) {
        let body = "All selected commits must be on the current branch".to_owned();
        refuse(this, cx, format!("Cannot {verb}"), body).await?;
        return Ok(None);
    }
    Ok(Some((range, repo)))
}

/// Undo Commit…: the latest commit of the current branch goes, its changes stay staged and its
/// message becomes the next commit's.
async fn undo_commit(this: This, cx: &mut AsyncWindowContext, commit: Commit) -> anyhow::Result<()> {
    refresh(&this, cx).await?;
    let (repo, branch) = vcs_state(&this, cx, |state| (state.repo.clone(), state.branch.clone()))?;
    if let Some(problem) = undo_commit_problem(&commit, branch.as_ref()) {
        return refuse(&this, cx, "Cannot undo commit".to_owned(), problem).await;
    }
    if head_pushed(branch.as_ref()) {
        let upstream = branch.as_ref().and_then(|branch| branch.upstream.as_deref());
        let request = ConfirmRequest::danger("Undo Commit", pushed_warning(upstream, "Undoing it"), "Undo Commit");
        if !confirm(&this, cx, request).await? {
            return Ok(());
        }
    }
    // The undone changes go to a change list the user picks (a new one is named after the commit).
    let chosen = this.update_in(cx, |w, window, cx| w.choose_target_changelist(&commit.summary, window, cx))?;
    let Ok(list) = chosen.await else { return Ok(()) };
    let (path, revision) = (repo_path(repo.as_deref()), commit.id.clone());
    let files =
        call(&this, cx, move |host, label| async move { git::vcs_commit_files(&host, &label, path, revision).await })
            .await?;
    // Files that cannot be listed stay in the list they fall into.
    let files = files.unwrap_or_else(|error| {
        tracing::debug!("the files of the undone commit cannot be listed: {error}");
        Vec::new()
    });
    let id = short(&commit.id);
    let (title, success) = texts(&this, cx, |i18n| {
        let undid = [Param::from(id.as_str()), Param::from(commit.summary.as_str())];
        let title = i18n.tx_built(&["Undo commit ", ""], &[Param::from(id.as_str())]);
        (title, i18n.tx_built(&["Undid commit ", " “", "”"], &undid))
    })?;
    let (path, target) = (repo_path(repo.as_deref()), commit.id.clone());
    let work =
        move |host: Arc<Host>, label: String| async move { git::vcs_undo_commit(&host, &label, path, target).await };
    let Some(message) = run_op(&this, cx, &title, Some(success), false, work).await? else { return Ok(()) };
    reload_log(&this, cx)?;
    // The undone commit's message becomes the next commit's.
    this.update(cx, |workbench, cx| {
        let root = repo.clone().or_else(|| workbench.vcs_state().and_then(|state| state.root.clone()));
        let paths: Vec<String> = files
            .iter()
            .flat_map(|file| std::iter::once(&file.path).chain(file.old_path.as_ref()))
            .map(|rel| repository_path(root.as_deref(), rel))
            .collect();
        workbench.move_dropped_changes(&paths, &list, cx);
        let state = workbench.commit_state(cx);
        state.update(cx, |state, cx| {
            state.message = message;
            cx.notify();
        });
    })
}

/// Edit Commit Message…: rewords one commit of the current branch without touching the index or
/// the working tree.
async fn edit_message(this: This, cx: &mut AsyncWindowContext, commit: Commit) -> anyhow::Result<()> {
    let Some((_, repo)) = range_from(&this, cx, &commit, "edit commit message").await? else { return Ok(()) };
    // A paged log carries the summary line only: the dialog edits the whole message.
    let initial = if commit.message_truncated {
        let (path, revision) = (repo_path(repo.as_deref()), commit.id.clone());
        let read =
            call(
                &this,
                cx,
                move |host, label| async move { git::vcs_log_details(&host, &label, path, revision).await },
            )
            .await?;
        read.map_or_else(|_| commit.message.clone(), |details| details.message)
    } else {
        commit.message.clone()
    };
    let id = short(&commit.id);
    let title = texts(&this, cx, |i18n| i18n.tx_built(&["Edit Commit Message (", ")"], &[Param::from(id.as_str())]))?;
    let Some(message) = message_dialog(&this, cx, title, initial.clone()).await? else { return Ok(()) };
    if message.trim() == initial.trim() {
        return Ok(());
    }
    let (op_title, changed_title, undone) = texts(&this, cx, |i18n| {
        let values = [Param::from(id.as_str())];
        (
            i18n.tx_built(&["Edit commit message of ", ""], &values),
            i18n.tx_built(&["Commit message of ", " changed"], &values),
            i18n.tx("Commit message change undone"),
        )
    })?;
    let (path, target) = (repo_path(repo.as_deref()), commit.id.clone());
    let work = move |host: Arc<Host>, label: String| async move {
        git::vcs_reword(&host, &label, path, target, message).await
    };
    let Some(change) = run_op(&this, cx, &op_title, None, false, work).await? else { return Ok(()) };
    reload_log(&this, cx)?;
    let key = format!("vcs.reword:{}", change.after);
    let head = UndoHead { repo, before: change.before, after: change.after, undone, key: key.clone() };
    this.update(cx, |workbench, cx| {
        let mut notice = vcs_notice(NotificationLevel::Info, changed_title);
        notice.key = Some(key);
        notice.actions = workbench.undo_link(head);
        workbench.notify(notice, cx);
    })
}

/// Settings › Version Control › Confirmation › Ask for confirmation to drop commits, with "Don't
/// ask again".
async fn confirm_drop(this: &This, cx: &mut AsyncWindowContext, title: String, body: String) -> anyhow::Result<bool> {
    let asks = this
        .update(cx, |workbench, cx| settings::effective(&workbench.label, cx).vcs.confirmation.confirm_drop_commits)?;
    if !asks {
        return Ok(true);
    }
    let request = AskRequest {
        title,
        body,
        warning: true,
        checkbox: Some("Don't ask again".to_owned()),
        buttons: vec![AskButton { id: "drop", label: "Drop", primary: true }],
        ..AskRequest::default()
    };
    let answer = ask(this, cx, request).await?;
    if answer.button != "drop" {
        return Ok(false);
    }
    if answer.checked {
        this.update(cx, |workbench, cx| {
            let key = "vcs.confirmation.confirmDropCommits";
            if let Err(error) = settings::set(&workbench.label, SettingsScope::Global, key, &Value::Bool(false), cx) {
                tracing::warn!("cannot store {key}: {}", error.message);
            }
        })?;
    }
    Ok(true)
}

/// Drop Commit / Drop Commits: removes `commits` (newest first) from the current branch.
async fn drop_commits(this: This, cx: &mut AsyncWindowContext, commits: Vec<Commit>) -> anyhow::Result<()> {
    if let [commit] = commits.as_slice() {
        let Some((range, _)) = range_from(&this, cx, commit, "drop commit").await? else { return Ok(()) };
        if range.commits.len() == 1 && range.base.is_none() {
            let body = "It is the only commit of the branch".to_owned();
            return refuse(&this, cx, "Cannot drop commit".to_owned(), body).await;
        }
        let id = short(&commit.id);
        let (title, body, done) = texts(&this, cx, |i18n| {
            let values = [Param::from(id.as_str()), Param::from(commit.summary.as_str())];
            let strings = ["Drop commit ", " “", "” from the current branch? Its changes are removed."];
            (i18n.tx("Drop Commit"), i18n.tx_built(&strings, &values), format!("Dropped commit {id}"))
        })?;
        if !confirm_drop(&this, cx, title, body).await? {
            return Ok(());
        }
        let plan = single_commit_plan(&range, &commit.id, RebaseAction::Drop, None);
        return run_rebase(&this, cx, range.base, plan, done, "Drop undone").await;
    }
    let verb = "drop commits";
    let Some((range, _)) = range_for(&this, cx, &commits, verb).await? else { return Ok(()) };
    let ids: HashSet<&str> = commits.iter().map(|commit| commit.id.as_str()).collect();
    if range.base.is_none() && range.commits.iter().all(|commit| ids.contains(commit.id.as_str())) {
        let body = "At least one commit of the branch must be kept".to_owned();
        return refuse(&this, cx, "Cannot drop commits".to_owned(), body).await;
    }
    let count = commits.len();
    let (title, body) = texts(&this, cx, |i18n| {
        let strings = ["Drop ", " commits from the current branch? Their changes are removed."];
        (i18n.tx("Drop Commits"), i18n.tx_built(&strings, &[Param::from(count)]))
    })?;
    if !confirm_drop(&this, cx, title, body).await? {
        return Ok(());
    }
    let plan = range
        .commits
        .iter()
        .map(|commit| {
            let action = if ids.contains(commit.id.as_str()) { RebaseAction::Drop } else { RebaseAction::Pick };
            RebaseEntry { commit: commit.id.clone(), action, message: None }
        })
        .collect();
    run_rebase(&this, cx, range.base, plan, format!("Dropped {count} commits"), "Drop undone").await
}

/// Squash Commits…: melds `commits` (newest first) into one, at the position of the oldest, with
/// the message typed in the dialog.
async fn squash_commits(this: This, cx: &mut AsyncWindowContext, commits: Vec<Commit>) -> anyhow::Result<()> {
    if commits.len() < 2 {
        return Ok(());
    }
    let Some((range, _)) = range_for(&this, cx, &commits, "squash commits").await? else { return Ok(()) };
    let ids: HashSet<String> = commits.iter().map(|commit| commit.id.clone()).collect();
    let initial: Vec<&str> =
        range.commits.iter().filter(|commit| ids.contains(&commit.id)).map(|commit| commit.message.trim()).collect();
    let count = commits.len();
    let title = texts(&this, cx, |i18n| i18n.tx_built(&["Squash ", " Commits"], &[Param::from(count)]))?;
    let Some(message) = message_dialog(&this, cx, title, initial.join("\n\n")).await? else { return Ok(()) };
    if message.trim().is_empty() {
        return Ok(());
    }
    let plan = squash_plan(&range, &ids, &message);
    run_rebase(&this, cx, range.base, plan, format!("Squashed {count} commits"), "Squash undone").await
}

/// Interactively Rebase from Here…: the rebase dialog for `target..HEAD`, then the plan made in it.
async fn interactive_rebase(this: This, cx: &mut AsyncWindowContext, target: Commit) -> anyhow::Result<()> {
    let Some((range, _)) = range_from(&this, cx, &target, "rebase").await? else { return Ok(()) };
    let (commits, base) = (range.commits.clone(), range.base.clone());
    let answer = this.update_in(cx, move |workbench, window, cx| {
        let dialog = cx.new(|cx| InteractiveRebaseDialog::new(commits, base, cx));
        workbench.open_answer_dialog(
            dialog,
            |event: &InteractiveRebaseEvent| {
                let InteractiveRebaseEvent::Closed(plan) = event;
                plan.clone()
            },
            window,
            cx,
        )
    })?;
    let Some(plan) = answer.await.ok().flatten() else { return Ok(()) };
    let done = texts(&this, cx, |i18n| i18n.tx("Rebase finished"))?;
    run_rebase(&this, cx, range.base, plan, done, "Rebase undone").await
}

/// Commit and Rebase after Fixup… / Squash Into…: the commit just made moves into `target` with the
/// autosquash of an interactive rebase from it.
pub(super) async fn rebase_after_fixup(this: This, cx: &mut AsyncWindowContext, target: Commit) -> anyhow::Result<()> {
    this.update(cx, |workbench, cx| {
        let state = workbench.commit_state(cx);
        state.update(cx, |state, cx| {
            state.fixup_target = None;
            cx.notify();
        });
    })?;
    let Some((range, _)) = range_from(&this, cx, &target, "rebase").await? else { return Ok(()) };
    let plan = to_entries(&initial_rows(&range.commits));
    let done = texts(&this, cx, |i18n| i18n.tx("Rebased"))?;
    run_rebase(&this, cx, range.base, plan, done, "Rebase undone").await
}

/// New Tag…: a lightweight tag on `commit`, named in a form that refuses a name in use.
async fn new_tag(this: This, cx: &mut AsyncWindowContext, commit: Commit) -> anyhow::Result<()> {
    let current = vcs_state(&this, cx, |state| state.repo.clone())?;
    let commit_id = commit.id.clone();
    let on = NewTagOn::new(&RepoCommit::from(commit), current.as_deref());
    let path = repo_path(on.repo());
    let tags = call(&this, cx, move |host, label| async move { git::vcs_tags(&host, &label, path).await }).await?;
    let existing = tags.unwrap_or_default();
    let request = texts(&this, cx, |i18n| on.prompt(i18n))?;
    let form = TagForm::on_commit(request, &commit_id, existing);
    let Some(FormAnswer::Tag { name, .. }) = ask_form(&this, cx, Box::new(form)).await? else { return Ok(()) };
    let create = texts(&this, cx, |i18n| on.create(&name, i18n))?;
    let (path, tag, target) = (repo_path(create.repo.as_deref()), create.name.clone(), create.commit.clone());
    let (message, force) = (create.message.clone(), create.force);
    let work = move |host: Arc<Host>, label: String| async move {
        git::vcs_create_tag(&host, &label, path, tag, target, message, force).await
    };
    if run_op(&this, cx, &create.title, Some(create.success.clone()), false, work).await?.is_some() {
        // Tags are not part of what makes the log read again.
        reload_log(&this, cx)?;
    }
    Ok(())
}

/// View in Browser: the commit's page on the web UI of the current branch's remote (or `origin`).
async fn open_in_browser(this: This, cx: &mut AsyncWindowContext, commit: Commit) -> anyhow::Result<()> {
    let (repo, upstream) = vcs_state(&this, cx, |state| {
        (state.repo.clone(), state.branch.as_ref().and_then(|branch| branch.upstream.clone()))
    })?;
    let remote = match upstream.as_deref().filter(|upstream| !upstream.is_empty()) {
        Some(upstream) => upstream.split_once('/').map_or(upstream, |(remote, _)| remote).to_owned(),
        None => "origin".to_owned(),
    };
    let (path, key) = (repo_path(repo.as_deref()), format!("remote.{remote}.url"));
    let url = call(&this, cx, move |host, label| async move { git::vcs_config_get(&host, &label, path, key).await })
        .await?
        .ok()
        .flatten();
    match url.and_then(|url| commit_web_url(&url, &commit.id)) {
        Some(page) => this.update(cx, |_, cx| cx.open_url(&page)),
        None => {
            let title = texts(&this, cx, |i18n| {
                i18n.tx_built(&["No web URL for remote '", "'"], &[Param::from(remote.as_str())])
            })?;
            notify(&this, cx, vcs_notice(NotificationLevel::Warning, title))
        }
    }
}

/// Selects the commit `rev` names (a hash, a branch or a tag) in the log. Whether there is one.
async fn go_to_ref(
    this: &This,
    cx: &mut AsyncWindowContext,
    rev: String,
    repo: Option<String>,
) -> anyhow::Result<bool> {
    let target = rev.trim().to_owned();
    if target.is_empty() {
        return Ok(false);
    }
    let path = repo_path(repo.as_deref());
    let query = LogQuery { branch: Some(target), limit: 1, ..LogQuery::default() };
    let found =
        call(this, cx, move |host, label| async move { git::vcs_log(&host, &label, path, query).await }).await?;
    let Some(id) = found.ok().and_then(|commits| commits.into_iter().next()).map(|commit| commit.id) else {
        return Ok(false);
    };
    // A log just shown is still being read.
    for _ in 0..200 {
        let loading =
            this.update(cx, |workbench, cx| workbench.git_log_view().is_some_and(|log| log.read(cx).is_loading()))?;
        if !loading {
            break;
        }
        cx.background_executor().timer(Duration::from_millis(25)).await;
    }
    this.update(cx, |workbench, cx| {
        workbench.git_log_view().is_some_and(|log| log.update(cx, |log, cx| log.select_rev(&id, cx)))
    })
}

/// [`go_to_ref`], saying so when there is no such commit.
async fn go_to_ref_or_say(
    this: This,
    cx: &mut AsyncWindowContext,
    rev: String,
    repo: Option<String>,
) -> anyhow::Result<()> {
    if go_to_ref(&this, cx, rev.clone(), repo).await? {
        return Ok(());
    }
    let title =
        texts(&this, cx, |i18n| i18n.tx_built(&["'", "' was not found in the log"], &[Param::from(rev.as_str())]))?;
    notify(&this, cx, vcs_notice(NotificationLevel::Warning, title))
}

/// Go to Hash/Branch/Tag: the Git tool window shows, the revision is asked for and selected.
async fn ask_go_to_ref(this: This, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let request = PromptRequest {
        title: "Go to Hash/Branch/Tag".to_owned(),
        label: Some("Enter hash or branch/tag name".to_owned()),
        mono: true,
        ..PromptRequest::default()
    };
    let rule = FieldRule::NonEmpty("Enter hash or branch/tag name".to_owned());
    let Some(rev) = prompt(&this, cx, request, rule).await? else { return Ok(()) };
    go_to_ref_or_say(this, cx, rev, None).await
}

// -------------------------------------------------------------------------------------------------
// Changed files actions

/// Get from Revision: the working files take their content after the change; a file the change
/// deleted is deleted.
async fn get_from_revision(
    this: This,
    cx: &mut AsyncWindowContext,
    changes: Vec<LogChange>,
    root: Option<String>,
) -> anyhow::Result<()> {
    let count = changes.len();
    let (message, title, success) = texts(&this, cx, |i18n| {
        let message = match changes.as_slice() {
            [one] => i18n.tx_built(
                &["Replace '", "' with its content from the revision? Local changes to it are lost."],
                &[Param::from(base_name(&one.path))],
            ),
            _ => i18n.tx_built(
                &["Replace ", " files with their content from the revision? Local changes to them are lost."],
                &[Param::from(count)],
            ),
        };
        let success = if count == 1 {
            i18n.tx("Got 1 file from revision")
        } else {
            i18n.tx_built(&["Got ", " files from revision"], &[Param::from(count)])
        };
        (message, i18n.tx("Get from Revision"), success)
    })?;
    if !confirm(&this, cx, ConfirmRequest::danger("Get from Revision", message, "Get")).await? {
        return Ok(());
    }
    save_all(&this, cx).await?;
    let files: Vec<(PathBuf, Option<String>)> = changes
        .iter()
        .map(|change| {
            let root = change.repo.as_deref().or(root.as_deref());
            (PathBuf::from(repository_path(root, &change.path)), change.after.clone())
        })
        .collect();
    let work = move |host: Arc<Host>, label: String| async move {
        for (path, after) in files {
            let Some(rev) = after else {
                if path.exists() {
                    crate::backend::delete_path(&host, &label, path).await?;
                }
                continue;
            };
            let Some(text) = git::vcs_file_at(&host, &label, path.clone(), Some(rev)).await? else { continue };
            if !path.exists() {
                ide_host::ops::fs::create_file(&host, &label, path.clone())?;
            }
            save_exact(&host, &label, path, text).await?;
        }
        Ok(count)
    };
    run_op(&this, cx, &title, Some(success), false, work).await?;
    Ok(())
}

/// Revert Selected Changes: the reverse of the selected files' changes in `commits` (newest first)
/// applied to the working tree.
async fn revert_changes(
    this: This,
    cx: &mut AsyncWindowContext,
    changes: Vec<LogChange>,
    commits: Vec<RepoCommit>,
    root: Option<String>,
) -> anyhow::Result<()> {
    let repo = changes.first().and_then(|change| change.repo.clone()).or_else(|| root.clone());
    save_all(&this, cx).await?;
    // Oldest first; the backend reverses the order for a reversed patch.
    let ids: Vec<String> = commits
        .iter()
        .rev()
        .filter(|commit| commit.repo.clone().or_else(|| root.clone()) == repo)
        .map(|commit| commit.commit.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let paths: BTreeSet<String> =
        changes.iter().flat_map(|change| std::iter::once(change.path.clone()).chain(change.old_path.clone())).collect();
    let (title, nothing) = texts(&this, cx, |i18n| (i18n.tx("Revert selected changes"), i18n.tx("Nothing to revert")))?;
    let path = repo_path(repo.as_deref());
    let work = move |host: Arc<Host>, label: String| async move {
        let source = PatchSource::Commits { commits: ids, reverse: true };
        let patch = git::vcs_create_patch(&host, &label, path.clone(), source).await?;
        let patch = filter_patch(&patch, &paths);
        if patch.trim().is_empty() {
            return Err(IdeError::invalid(nothing));
        }
        git::vcs_apply_patch(&host, &label, path, patch).await
    };
    let Some(outcome) = run_op(&this, cx, &title, None, false, work).await? else { return Ok(()) };
    let conflicted = vcs_state(&this, cx, |state| !state.conflicted_files().is_empty())?;
    if !outcome.conflicts.is_empty() || conflicted {
        let n = outcome.conflicts.len();
        let mut notice = vcs_notice(
            NotificationLevel::Warning,
            format!("Changes reverted: {n} file{} with conflicts", if n == 1 { "" } else { "s" }),
        );
        notice.body = Some(outcome.conflicts.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
        let resolve = NotificationAction { label: "Resolve…".to_owned(), command: RESOLVE_CONFLICTS.to_owned() };
        notice.actions = vec![resolve];
        notify(&this, cx, notice)?;
        return this.update_in(cx, |_, window, cx| run_action_later(RESOLVE_CONFLICTS.to_owned(), None, window, cx));
    }
    let n = outcome.applied.len();
    let title = format!("Changes reverted: {n} file{} changed", if n == 1 { "" } else { "s" });
    notify(&this, cx, vcs_notice(NotificationLevel::Info, title))
}

/// The commits of `repo`'s current branch that are not on its upstream (`@{u}..HEAD`); `None` when
/// the branch tracks no upstream or the range cannot be read, so that no commit counts as pushed.
async fn outgoing_commits(
    this: &This,
    cx: &mut AsyncWindowContext,
    repo: Option<String>,
) -> anyhow::Result<Option<HashSet<String>>> {
    let path = repo_path(repo.as_deref());
    let read = call(this, cx, move |host, label| async move {
        let info = git::vcs_branch_info(&host, &label, path.clone()).await?;
        if info.upstream.as_deref().is_none_or(str::is_empty) {
            return Ok(None);
        }
        let query = LogQuery {
            branch: Some("HEAD".to_owned()),
            range: Some("@{u}..HEAD".to_owned()),
            limit: 0,
            ..LogQuery::default()
        };
        let commits = git::vcs_log(&host, &label, path, query).await?;
        Ok(Some(commits.into_iter().map(|commit| commit.id).collect::<HashSet<String>>()))
    })
    .await?;
    // A missing upstream does not keep the menu from showing.
    Ok(read.unwrap_or_else(|error| {
        tracing::debug!("the outgoing commits cannot be read: {error}");
        None
    }))
}

// -------------------------------------------------------------------------------------------------
// The workbench

impl Workbench {
    /// The commits a commit action acts on: those `arg` names (a key or a list of keys), else the
    /// log's selection; newest first as the log lists them.
    fn log_commits(&self, arg: Option<&Value>, cx: &App) -> Vec<Commit> {
        let Some(log) = self.git_log_view() else { return Vec::new() };
        let log = log.read(cx);
        let ids = match arg_commit_ids(arg) {
            ids if ids.is_empty() => log.selected().map(|commit| commit.id.clone()).into_iter().collect(),
            ids => ids,
        };
        log.commits().iter().filter(|commit| ids.contains(&commit.id)).cloned().collect()
    }

    /// Shows `dialog` in the dialog layer; the receiver gets what `answer` makes of the event it
    /// closes with. A dialog replaced by another drops the sender: the receiver fails.
    fn open_answer_dialog<V, E, T>(
        &mut self,
        dialog: Entity<V>,
        answer: fn(&E) -> Option<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Option<T>>
    where
        V: EventEmitter<E> + Focusable + Render,
        E: 'static,
        T: 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut sender = Some(sender);
        let events = cx.subscribe_in(&dialog, window, move |workbench, _, event: &E, window, cx| {
            workbench.close_vcs_dialog(window, cx);
            if let Some(sender) = sender.take() {
                // The operation may have ended meanwhile.
                let _ = sender.send(answer(event));
            }
        });
        let focus = dialog.focus_handle(cx);
        self.open_vcs_dialog(dialog.into(), focus, events, window, cx);
        receiver
    }

    fn run_commit_action(
        &mut self,
        action: CommitAction,
        arg: Option<&Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let commits = self.log_commits(arg, cx);
        let ids = arg_commit_ids(arg);
        if commits.is_empty() && !ids.is_empty() {
            // Commits the log has not read (or a log not shown yet) are read from the repository.
            let repo = self.vcs_state().and_then(|state| state.repo.clone());
            cx.spawn_in(window, async move |this, cx| {
                let acted = async {
                    let commits = read_commits(&this, cx, repo, ids).await?;
                    this.update_in(cx, |w, window, cx| w.act_on_commits(action, commits, window, cx))
                };
                finished(acted.await);
            })
            .detach();
            return;
        }
        self.act_on_commits(action, commits, window, cx);
    }

    /// Runs `action` on `commits` (newest first), or says that a commit is to be selected.
    fn act_on_commits(
        &mut self,
        action: CommitAction,
        mut commits: Vec<Commit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if commits.is_empty() {
            self.notify(vcs_notice(NotificationLevel::Info, "Select a commit in the Git log"), cx);
            return;
        }
        if !action.many() {
            commits.truncate(1);
        }
        let commit = commits[0].clone();
        match action {
            CommitAction::CopyRevision => {
                let ids: Vec<&str> = commits.iter().map(|commit| commit.id.as_str()).collect();
                cx.write_to_clipboard(ClipboardItem::new_string(ids.join(" ")));
            }
            CommitAction::CreatePatch => self.open_create_patch_for_commits(commits, window, cx),
            CommitAction::CherryPick => {
                cx.spawn_in(window, async move |this, cx| finished(cherry_pick(this, cx, commits).await)).detach()
            }
            CommitAction::Revert => {
                cx.spawn_in(window, async move |this, cx| finished(revert(this, cx, commits).await)).detach()
            }
            CommitAction::CheckoutRevision => {
                let repo = self.vcs_state().and_then(|state| state.repo.clone());
                let success = Success::Fixed(format!("Checked out {} (detached HEAD)", commit.short_id));
                cx.spawn_in(window, async move |this, cx| {
                    finished(checkout(&this, cx, commit.id, repo, success).await.map(|_| ()))
                })
                .detach();
            }
            CommitAction::CompareWithLocal => {
                cx.spawn_in(window, async move |this, cx| finished(compare(this, cx, commit, None).await)).detach()
            }
            CommitAction::CompareVersions => {
                let [newer, older] = commits.as_slice() else {
                    self.notify(vcs_notice(NotificationLevel::Info, "Select two commits to compare"), cx);
                    return;
                };
                // Newest first, as the log shows them.
                let (older, newer) = (older.clone(), newer.clone());
                cx.spawn_in(window, async move |this, cx| finished(compare(this, cx, older, Some(newer)).await))
                    .detach()
            }
            CommitAction::ResetToHere => {
                cx.spawn_in(window, async move |this, cx| finished(reset_to(this, cx, commit).await)).detach()
            }
            CommitAction::UndoCommit => {
                cx.spawn_in(window, async move |this, cx| finished(undo_commit(this, cx, commit).await)).detach()
            }
            CommitAction::EditMessage => {
                cx.spawn_in(window, async move |this, cx| finished(edit_message(this, cx, commit).await)).detach()
            }
            CommitAction::DropCommit => {
                cx.spawn_in(window, async move |this, cx| finished(drop_commits(this, cx, commits).await)).detach()
            }
            CommitAction::SquashCommits => {
                cx.spawn_in(window, async move |this, cx| finished(squash_commits(this, cx, commits).await)).detach()
            }
            CommitAction::Fixup => self.prepare_fixup(commit, "fixup", window, cx),
            CommitAction::SquashInto => self.prepare_fixup(commit, "squash", window, cx),
            CommitAction::PushUpToHere => {
                let head = self.vcs_state().and_then(|state| state.branch.as_ref()).and_then(|info| info.head.clone());
                if let Some(head) = head.filter(|head| !head.is_empty()) {
                    let arg = json!({ "branch": head, "revision": commit.id });
                    run_action_later("vcs.push".to_owned(), Some(arg), window, cx);
                }
            }
            CommitAction::RebaseOntoCommit => {
                let command = BranchCommand::RebaseOnto { branch: commit.id.clone(), reference: commit.id };
                self.run_branch_command(BranchesCommand { target: RepoTarget::Selected, command }, window, cx);
            }
            CommitAction::NewBranch => {
                let command = BranchCommand::NewBranch { from: Some(commit.id) };
                self.run_branch_command(BranchesCommand { target: RepoTarget::Selected, command }, window, cx);
            }
            CommitAction::NewTag => {
                cx.spawn_in(window, async move |this, cx| finished(new_tag(this, cx, commit).await)).detach()
            }
            CommitAction::GoToParent => self.go_to_relative(&commit, true, window, cx),
            CommitAction::GoToChild => self.go_to_relative(&commit, false, window, cx),
            CommitAction::OpenInBrowser => {
                cx.spawn_in(window, async move |this, cx| finished(open_in_browser(this, cx, commit).await)).detach()
            }
            CommitAction::InteractiveRebase => {
                cx.spawn_in(window, async move |this, cx| finished(interactive_rebase(this, cx, commit).await)).detach()
            }
        }
    }

    /// The commit context menu of the log's selected commit, at `position`, once the commits not yet
    /// on the current branch's upstream are read: IDEA's rows, each an action on the commit or a row of
    /// a branch's menu. A commit that is on the upstream has its history rewriting rows disabled.
    pub(super) fn show_commit_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.vcs_state().and_then(|state| state.repo.clone());
        cx.spawn_in(window, async move |this, cx| {
            let shown = async {
                let outgoing = outgoing_commits(&this, cx, repo).await?;
                this.update_in(cx, |workbench, window, cx| workbench.open_commit_menu(position, outgoing, window, cx))
            };
            finished(shown.await);
        })
        .detach();
    }

    /// [`Workbench::show_commit_menu`] with the outgoing commits read: `None` without an upstream.
    fn open_commit_menu(
        &mut self,
        position: Point<Pixels>,
        outgoing: Option<HashSet<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(log) = self.git_log_view() else { return };
        let Some(commit) = log.read(cx).selected().cloned() else { return };
        let Some(vcs) = self.vcs_state().cloned() else { return };
        let (_, favorites) = self.vcs_tags_and_favorites();
        // The branches whose tip is the commit bring their own submenus.
        let ref_branches = vcs.branches.iter().filter(|branch| branch.commit == commit.id).cloned().collect();
        let selected = RepoCommit { ref_branches, ..RepoCommit::from(commit) };
        let mut model = LogModel::default();
        model.outgoing = outgoing;
        let context = CommitMenuContext { log: &model, vcs: &vcs, bookmarked: false, favorites: favorites.as_deref() };
        let i18n = &mut cx.default_global::<Localization>().0;
        let items = commit_menu(&selected, std::slice::from_ref(&selected), context, i18n);
        self.show_menu(
            &items,
            position,
            |workbench, command: LogCommand, window, cx| match command {
                LogCommand::Branch { repo, command } => {
                    let target = repo.map_or(RepoTarget::Selected, RepoTarget::Repo);
                    workbench.run_branch_command(BranchesCommand { target, command }, window, cx);
                }
                LogCommand::CopyCommitMessage(key) => {
                    let commits = workbench.log_commits(Some(&Value::String(key)), cx);
                    if let Some(commit) = commits.first() {
                        cx.write_to_clipboard(ClipboardItem::new_string(commit.message.trim_end().to_owned()));
                    }
                }
                other => tracing::info!("{other:?} of the commit menu is not available"),
            },
            window,
            cx,
        );
    }

    /// Create Patch… of commits: the Create Patch dialog on the changes `commits` (newest first)
    /// made, suggesting a file in the project's root named after them.
    fn open_create_patch_for_commits(&mut self, commits: Vec<Commit>, window: &mut Window, cx: &mut Context<Self>) {
        let vcs = self.vcs_state().cloned().unwrap_or_default();
        let root = vcs.root.clone().unwrap_or_default();
        let file = format!("{root}/{}", suggested_patch_name(PatchOrigin::Commits(&commits), ""));
        let described = match commits.as_slice() {
            [one] => format!("{} {}", one.short_id, one.summary),
            many => format!("{} commits", many.len()),
        };
        // The patch applies the commits oldest first.
        let mut ordered = commits;
        ordered.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));
        let from = PatchMade::Repo {
            repo: vcs.current_repo_root().map(str::to_owned),
            commits: ordered.into_iter().map(|commit| commit.id).collect(),
        };
        self.open_create_patch(CreatePatchSource::Described(described), &file, true, from, window, cx);
    }

    /// Fixup… / Squash Into…: a `fixup!` / `squash!` message of the local changes for `target` in the
    /// commit interface; the next interactive rebase moves that commit into the target.
    fn prepare_fixup(&mut self, target: Commit, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.commit_state(cx);
        state.update(cx, |state, cx| {
            state.message = format!("{kind}! {}", target.summary);
            state.fixup_target = Some(target);
            cx.notify();
        });
        run_action_later("vcs.commit".to_owned(), None, window, cx);
    }

    /// Go to Parent / Child Commit: the loaded parent (or child) of `commit` is selected; with several,
    /// the one chosen from a menu.
    fn go_to_relative(&mut self, commit: &Commit, parent: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(log) = self.git_log_view() else { return };
        let candidates: Vec<Commit> = log
            .read(cx)
            .commits()
            .iter()
            .filter(
                |other| {
                    if parent { commit.parents.contains(&other.id) } else { other.parents.contains(&commit.id) }
                },
            )
            .cloned()
            .collect();
        match candidates.as_slice() {
            [] => {}
            [one] => {
                let id = one.id.clone();
                log.update(cx, |log, cx| log.select_rev(&id, cx));
            }
            several => {
                let items: Vec<_> = several
                    .iter()
                    .map(|other| MenuItem::item(format!("{} {}", other.short_id, other.summary), other.id.clone()))
                    .collect();
                let size = window.viewport_size();
                let position = gpui_kit::point(size.width / 2.0 - px(150.0), size.height / 3.0);
                self.show_menu(
                    &items,
                    position,
                    move |workbench, id: String, _, cx| {
                        if let Some(log) = workbench.git_log_view() {
                            log.update(cx, |log, cx| log.select_rev(&id, cx));
                        }
                    },
                    window,
                    cx,
                );
            }
        }
    }

    /// Go to Hash/Branch/Tag: shows the Git tool window and asks for the revision.
    fn start_go_to_ref(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_layout(window, cx, |layout| layout.show(GIT_TOOL_WINDOW, true));
        cx.spawn_in(window, async move |this, cx| finished(ask_go_to_ref(this, cx).await)).detach();
    }

    /// Show Commit Bookmark: `git-log:<repository>#<commit>` selects the commit in the log of the
    /// project's repository; a repository outside the project has no log here.
    fn reveal_bookmark(&mut self, arg: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let bookmark = arg.and_then(|arg| arg.strip_prefix("git-log:"));
        let Some((repo, id)) = bookmark.and_then(|rest| rest.split_once('#')) else { return };
        if id.is_empty() {
            return;
        }
        let Some(repo) = decode_uri_component(repo) else { return };
        let roots = self.vcs_state().map(|state| state.roots.clone()).unwrap_or_default();
        if !repo.is_empty() && !roots.contains(&repo) {
            tracing::info!("the bookmarked commit {id} is in {repo}, which has no log in this window");
            return;
        }
        self.with_layout(window, cx, |layout| layout.show(GIT_TOOL_WINDOW, true));
        let (id, repo) = (id.to_owned(), Some(repo).filter(|repo| !repo.is_empty()));
        cx.spawn_in(window, async move |this, cx| finished(go_to_ref(&this, cx, id, repo).await.map(|_| ()))).detach();
    }

    /// Collapse / Expand Linear Branches, Group by Directory, Show Diff Preview: the log's options
    /// with `change`, stored and shown.
    fn update_log_options(&mut self, change: impl FnOnce(&mut LogOptions), cx: &mut Context<Self>) {
        let mut options = match self.git_log_view() {
            Some(log) => log.read(cx).options().clone(),
            None => stored_log_options(cx),
        };
        change(&mut options);
        self.set_log_options(options, cx);
    }

    /// The Branches pane's selected refs.
    fn pane_targets(&self, cx: &App) -> Vec<BranchTarget> {
        let pane = self.git_log_view().and_then(|log| log.read(cx).branches_pane().cloned());
        pane.map(|pane| pane.read(cx).targets()).unwrap_or_default()
    }

    fn run_pane_action(
        &mut self,
        action: PaneAction,
        arg: Option<&Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.pane_targets(cx);
        let target = branch_arg(arg, &selected);
        let in_repo = |target: &BranchTarget| target.repo.clone().map_or(RepoTarget::Selected, RepoTarget::Repo);
        let command = match action {
            PaneAction::New => {
                let from = new_branch_source(target.as_ref()).map(str::to_owned);
                let repo = target.as_ref().map_or(RepoTarget::Selected, in_repo);
                Some((repo, BranchCommand::NewBranch { from }))
            }
            PaneAction::Update => {
                target.as_ref().filter(|target| can_update_branch(Some(*target))).and_then(|target| {
                    let branch = target.branch.as_ref()?;
                    Some((in_repo(target), BranchCommand::Update(branch.name.clone())))
                })
            }
            PaneAction::Delete => {
                let targets = branch_targets(arg, &selected);
                let deletable: Vec<BranchTarget> =
                    targets.into_iter().filter(|target| can_delete_branch(Some(target))).collect();
                (!deletable.is_empty()).then(|| (RepoTarget::Selected, BranchCommand::DeleteSelection(deletable)))
            }
            PaneAction::ToggleFavorite => target.as_ref().and_then(|target| {
                let branch = target.branch.as_ref()?;
                Some((in_repo(target), BranchCommand::ToggleFavorite(branch.name.clone())))
            }),
            PaneAction::Navigate => {
                let Some(target) = target else { return };
                let (rev, repo) = (target.ref_name, target.repo);
                cx.spawn_in(window, async move |this, cx| finished(go_to_ref_or_say(this, cx, rev, repo).await))
                    .detach();
                None
            }
        };
        if let Some((target, command)) = command {
            self.run_branch_command(BranchesCommand { target, command }, window, cx);
        }
    }

    /// The changed files an action acts on: those its argument names (repository-relative paths),
    /// else the pane's selection.
    fn log_changes_arg(&self, arg: Option<&Value>, cx: &App) -> Vec<LogChange> {
        let Some(pane) = self.log_details() else { return Vec::new() };
        let pane = pane.read(cx);
        match arg {
            Some(Value::Array(paths)) => {
                let paths: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
                pane.all_changes().iter().filter(|change| paths.contains(&change.path.as_str())).cloned().collect()
            }
            _ => pane.selected_changes(),
        }
    }

    fn run_log_changes_action(
        &mut self,
        command: ChangesCommand,
        changes: Vec<LogChange>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if changes.is_empty() {
            return;
        }
        match command {
            ChangesCommand::JumpToSource => {
                for change in changes.into_iter().filter(|change| change.after.is_some() && !change.read_only) {
                    self.run_log_changes_command(ChangesCommand::JumpToSource, vec![change], window, cx);
                }
            }
            ChangesCommand::GetFromRevision => self.get_from_revision(changes, window, cx),
            ChangesCommand::Revert => self.revert_log_changes(changes, window, cx),
            command => self.run_log_changes_command(command, changes, window, cx),
        }
    }

    /// Get from Revision on the changed files `changes`.
    pub(super) fn get_from_revision(&mut self, changes: Vec<LogChange>, window: &mut Window, cx: &mut Context<Self>) {
        if changes.is_empty() || changes.iter().any(|change| change.read_only) {
            return;
        }
        let root = self.vcs_state().and_then(|state| state.current_repo_root().map(str::to_owned));
        cx.spawn_in(window, async move |this, cx| finished(get_from_revision(this, cx, changes, root).await)).detach();
    }

    /// Revert Selected Changes on the changed files `changes` of the commits the pane shows.
    pub(super) fn revert_log_changes(&mut self, changes: Vec<LogChange>, window: &mut Window, cx: &mut Context<Self>) {
        if changes.is_empty() || changes.iter().any(|change| change.read_only) {
            return;
        }
        let root = self.vcs_state().and_then(|state| state.current_repo_root().map(str::to_owned));
        let repos: HashSet<Option<String>> =
            changes.iter().map(|change| change.repo.clone().or_else(|| root.clone())).collect();
        if repos.len() > 1 {
            return;
        }
        let commits = self.log_details().map(|pane| pane.read(cx).input().commits.clone()).unwrap_or_default();
        if commits.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| finished(revert_changes(this, cx, changes, commits, root).await))
            .detach();
    }

    /// Registers the Undo button of a history rewrite; the button, unless the rewrite moved nothing.
    fn undo_link(&mut self, head: UndoHead) -> Vec<NotificationAction> {
        if head.before.is_empty() || head.after.is_empty() || head.before == head.after {
            return Vec::new();
        }
        let command = format!("{UNDO_PREFIX}{}", head.key);
        self.log_actions.undo.retain(|(own, _)| *own != command);
        self.log_actions.undo.push((command.clone(), head));
        vec![NotificationAction { label: "Undo".to_owned(), command }]
    }

    /// The Undo button of a history rewrite's notification: HEAD goes back to where the rewrite found
    /// it, while it is still where the rewrite left it. Whether `command` was one.
    pub(super) fn run_log_undo(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(at) = self.log_actions.undo.iter().position(|(own, _)| own == command) else { return false };
        let (_, head) = self.log_actions.undo.remove(at);
        cx.spawn_in(window, async move |this, cx| {
            let undo = async {
                let title = texts(&this, cx, |i18n| i18n.tx("Undo"))?;
                let UndoHead { repo, before, after, undone, key } = head;
                let path = repo_path(repo.as_deref());
                let work = move |host: Arc<Host>, label: String| async move {
                    git::vcs_restore_head(&host, &label, path, before, after).await
                };
                if run_op(&this, cx, &title, None, false, work).await?.is_some() {
                    reload_log(&this, cx)?;
                    let mut notice = vcs_notice(NotificationLevel::Info, undone);
                    notice.key = Some(key);
                    notify(&this, cx, notice)?;
                }
                anyhow::Ok(())
            };
            finished(undo.await);
        })
        .detach();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::COMMIT_TOOL_WINDOW;
    use super::super::tests::{
        branch_label, click_on, git_in, git_project, init_repo, notice_titled, notification_titles, retype, run_action,
        wait_for_notice, wait_for_prompt, wait_long,
    };
    use super::*;
    use crate::platform::actions::action_toggled;
    use crate::vcs::change_dialogs::ChangelistDialog;
    use crate::vcs::git_form_dialog::GitFormDialog;
    use crate::vcs::log_view::GitLog;
    use crate::vcs::patch_dialogs::CreatePatchDialog;
    use crate::vcs_more::log_presentation::{LogFragments, present_log_graph};
    use crate::workbench::notification_balloons::EntryAction;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use ide_vcs::ResetMode;
    use std::path::Path;

    fn commit_file(root: &Path, file: &str, text: &str, message: &str) {
        if let Some(dir) = root.join(file).parent() {
            std::fs::create_dir_all(dir).expect("the folder is made");
        }
        std::fs::write(root.join(file), text).expect("the file is written");
        git_in(root, &["add", "."]);
        git_in(root, &["commit", "-q", "-m", message]);
    }

    fn rev(root: &Path, name: &str) -> String {
        git_in(root, &["rev-parse", name])
    }

    fn subjects_in(root: &Path) -> Vec<String> {
        git_in(root, &["log", "--format=%s"]).lines().map(str::to_owned).collect()
    }

    fn read(root: &Path, file: &str) -> String {
        std::fs::read_to_string(root.join(file)).expect("the file is read")
    }

    /// `vcsLogFocus` is on while the log has the keyboard and off once an editor has it. The log's
    /// keys follow: ⌥⇧⌘C copies the selected revision and ⌘F asks Go to Hash/Branch/Tag in the log;
    /// in the editor neither runs (⌘F is the editor's Find there).
    #[gpui_kit::test]
    async fn the_logs_keys_work_while_the_log_has_the_keyboard(cx: &mut TestAppContext) {
        use crate::platform::context::window_context;
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "a.txt", "two\n", "Second change");
        });
        let log_focus =
            |cx: &mut VisualTestContext| cx.update(|window, cx| window_context(window, cx).is_truthy("vcsLogFocus"));
        let log = show_log(&workbench, 2, cx);
        log.update(cx, |log, cx| log.select(0, cx));
        workbench.update_in(cx, |_, window, cx| window.focus(&log.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(log_focus(cx), "the log has the keyboard");
        cx.simulate_keystrokes("cmd-alt-shift-c");
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some(rev(&root, "HEAD")));
        cx.simulate_keystrokes("cmd-f");
        let prompt = wait_for_prompt(&workbench, cx);
        assert_eq!(prompt.read_with(cx, |dialog, _| dialog.title().to_owned()), "Go to Hash/Branch/Tag");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("a.txt"), window, cx));
        wait_long(cx, "the file opens", |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        workbench.update_in(cx, |_, window, cx| window.focus(&editor.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(!log_focus(cx), "the editor has the keyboard");
        cx.write_to_clipboard(ClipboardItem::new_string("before".to_owned()));
        cx.simulate_keystrokes("cmd-alt-shift-c");
        let clipboard = |cx: &mut VisualTestContext| cx.read_from_clipboard().and_then(|item| item.text());
        assert_ne!(clipboard(cx), Some(rev(&root, "HEAD")), "the log's Copy Revision Number does not run");
        // The key is the editor's Copy Reference, which copies the caret's place once it has read it.
        wait_long(cx, "the editor's Copy Reference", |cx| clipboard(cx).as_deref() == Some("a.txt:1"));
        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.vcs_prompt().is_none()), "no Go to prompt");
    }

    /// A commit action named with a commit the log has not read, here before the Git window was
    /// ever shown, reads it from the repository: Copy Revision Number copies it, Cherry-Pick applies
    /// it.
    #[gpui_kit::test]
    async fn a_commit_action_reads_a_commit_the_log_has_not_read(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["checkout", "-q", "-b", "topic"]);
            commit_file(root, "t.txt", "topic\n", "On topic");
            git_in(root, &["checkout", "-q", "main"]);
        });
        assert!(workbench.read_with(cx, |w, _| w.git_log_view().is_none()), "the log was never shown");
        let topic = rev(&root, "topic");
        run_action(cx, "vcs.log.copyRevision", Some(json!(topic)));
        wait_long(cx, "the revision is copied", |cx| {
            cx.read_from_clipboard().and_then(|item| item.text()).as_deref() == Some(topic.as_str())
        });
        run_action(cx, "vcs.log.cherryPick", Some(json!(topic)));
        wait_long(cx, "the commit is picked", |_| subjects_in(&root).first().map(String::as_str) == Some("On topic"));
        assert_eq!(read(&root, "t.txt"), "topic\n");
    }

    /// Shows the Git tool window and waits until its log lists at least `count` commits.
    fn show_log(workbench: &Entity<Workbench>, count: usize, cx: &mut VisualTestContext) -> Entity<GitLog> {
        run_action(cx, "toolwindow.git", None);
        let log = workbench.read_with(cx, |w, _| w.git_log_view()).expect("the Git window holds the log");
        wait_long(cx, "the log is read", |cx| {
            log.read_with(cx, |log, _| !log.is_loading() && log.commits().len() >= count)
        });
        log
    }

    fn log_subjects(log: &Entity<GitLog>, cx: &mut VisualTestContext) -> Vec<String> {
        log.read_with(cx, |log, _| log.commits().iter().map(|commit| commit.summary.clone()).collect())
    }

    fn selected(log: &Entity<GitLog>, cx: &mut VisualTestContext) -> Option<String> {
        log.read_with(cx, |log, _| log.selected().map(|commit| commit.id.clone()))
    }

    fn wait_for_dialog<V: 'static>(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Entity<V> {
        wait_long(cx, "the dialog", |cx| workbench.read_with(cx, |w, _| w.vcs_dialog_view::<V>().is_some()));
        workbench.read_with(cx, |w, _| w.vcs_dialog_view::<V>()).expect("the dialog is open")
    }

    /// Answers the commit message dialog with `message`.
    fn answer_message(dialog: &Entity<MessageDialog>, message: &str, cx: &mut VisualTestContext) {
        let area = dialog.read_with(cx, |dialog, _| dialog.area().clone());
        area.update(cx, |area, cx| area.set_text(message, cx));
        dialog.update(cx, |dialog, cx| dialog.press(crate::vcs::log_message_dialog::OK, cx));
        cx.run_until_parked();
    }

    fn commit_message(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> String {
        workbench.update(cx, |w, cx| w.commit_state(cx).read(cx).message.clone())
    }

    /// `main` holds "first" and "main side" (a.txt reads "main"); `feature` holds "feature work"
    /// (f.txt) and then "feature side" (a.txt reads "feature").
    fn feature_branch(root: &Path) {
        init_repo(root);
        git_in(root, &["checkout", "-q", "-b", "feature"]);
        commit_file(root, "f.txt", "feature\n", "feature work");
        commit_file(root, "a.txt", "feature\n", "feature side");
        git_in(root, &["checkout", "-q", "main"]);
        commit_file(root, "a.txt", "main\n", "main side");
    }

    /// Cherry-Pick applies a commit of another branch onto the current one; a commit that conflicts
    /// stops the cherry-pick and opens the Conflicts dialog instead of failing.
    #[gpui_kit::test]
    async fn cherry_pick_applies_a_commit_of_another_branch_and_a_conflicting_one_opens_the_conflicts(
        cx: &mut TestAppContext,
    ) {
        let (workbench, cx, _folders, root) = git_project(cx, feature_branch);
        show_log(&workbench, 4, cx);
        let work = rev(&root, "feature~1");
        run_action(cx, "vcs.log.cherryPick", Some(json!([work])));
        wait_for_notice(&workbench, &format!("Cherry-picked {} “feature work”", short(&work)), cx);
        assert_eq!(read(&root, "f.txt"), "feature\n");
        assert_eq!(subjects_in(&root)[0], "feature work");

        run_action(cx, "vcs.log.cherryPick", Some(json!(rev(&root, "feature"))));
        wait_long(cx, "the Conflicts dialog", |cx| workbench.read_with(cx, |w, _| w.conflicts_dialog().is_some()));
        assert!(root.join(".git/CHERRY_PICK_HEAD").exists(), "the cherry-pick is stopped");
        assert!(
            !notification_titles(&workbench, cx).contains(&"Cherry-pick failed".to_owned()),
            "a stop on conflicts is not a failure"
        );
    }

    /// Revert Commit adds a commit that undoes the reverted one.
    #[gpui_kit::test]
    async fn revert_commit_adds_a_commit_that_undoes_it(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "two\n", "add b");
        });
        show_log(&workbench, 2, cx);
        let added = rev(&root, "HEAD");
        run_action(cx, "vcs.log.revert", Some(json!(added)));
        wait_for_notice(&workbench, &format!("Reverted {} “add b”", short(&added)), cx);
        assert!(!root.join("b.txt").exists(), "the reverted file is gone");
        assert_eq!(subjects_in(&root)[0], "Revert \"add b\"");
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "");
    }

    /// Reset Current Branch to Here… asks for the mode: Soft keeps the differences staged, Mixed
    /// unstaged, Hard reverts the files, Keep reverts them but keeps the local changes.
    #[gpui_kit::test]
    async fn reset_current_branch_to_here_moves_the_branch_in_each_mode(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "n.txt", "n\n", "notes");
            commit_file(root, "a.txt", "two\n", "two");
            commit_file(root, "a.txt", "three\n", "three");
            // The commits stay in the log whatever main is reset to.
            git_in(root, &["branch", "keep"]);
        });
        show_log(&workbench, 4, cx);
        let (notes, two, three) = (rev(&root, "HEAD~2"), rev(&root, "HEAD~1"), rev(&root, "HEAD"));
        let reset = |commit: &str, mode: ResetMode, cx: &mut VisualTestContext| {
            run_action(cx, "vcs.log.resetToHere", Some(json!(commit)));
            let dialog = wait_for_dialog::<GitFormDialog>(&workbench, cx);
            let rows = dialog.update(cx, |dialog, cx| dialog.spec(cx).rows);
            let chosen = rows.iter().find_map(|row| match row {
                crate::vcs::git_forms::FormRow::Radio { id: "mode", value, .. } => Some(value.clone()),
                _ => None,
            });
            assert_eq!(chosen.as_deref(), Some("mixed"), "Mixed is chosen first");
            dialog.update(cx, |dialog, cx| {
                dialog.choose("mode", reset_mode_name(mode), cx);
                dialog.press(crate::vcs::git_forms::OK, cx);
            });
            wait_for_notice(&workbench, &format!("Reset main to {} ({})", short(commit), reset_mode_name(mode)), cx);
        };

        reset(&two, ResetMode::Soft, cx);
        assert_eq!(rev(&root, "HEAD"), two);
        assert_eq!(git_in(&root, &["diff", "--cached", "--name-only"]), "a.txt", "the difference is staged");
        assert_eq!(read(&root, "a.txt"), "three\n");

        reset(&notes, ResetMode::Mixed, cx);
        assert_eq!(rev(&root, "HEAD"), notes);
        assert_eq!(git_in(&root, &["diff", "--cached", "--name-only"]), "", "nothing is staged");
        assert_eq!(read(&root, "a.txt"), "three\n");

        reset(&three, ResetMode::Hard, cx);
        assert_eq!(rev(&root, "HEAD"), three);
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "");

        std::fs::write(root.join("n.txt"), "local\n").expect("the file is written");
        reset(&two, ResetMode::Keep, cx);
        assert_eq!(rev(&root, "HEAD"), two);
        assert_eq!(read(&root, "a.txt"), "two\n");
        assert_eq!(read(&root, "n.txt"), "local\n", "the local change is kept");
    }

    /// Undo Commit… is refused for an older commit; on the latest one its changes come back staged and
    /// its message is offered to the next commit.
    #[gpui_kit::test]
    async fn undo_commit_brings_the_changes_back_and_offers_the_message(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "work\n", "Add b\n\nWith a body");
        });
        show_log(&workbench, 2, cx);
        let (first, head) = (rev(&root, "HEAD~1"), rev(&root, "HEAD"));
        run_action(cx, "vcs.log.undoCommit", Some(json!(first)));
        let refused = wait_for_prompt(&workbench, cx);
        assert_eq!(refused.read_with(cx, |dialog, _| dialog.title().to_owned()), "Cannot undo commit");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(rev(&root, "HEAD"), head, "nothing is undone");

        run_action(cx, "vcs.log.undoCommit", Some(json!(head)));
        // The undone changes go to a list chosen first: a new one, named after the commit.
        let chooser = wait_for_dialog::<ChangelistDialog>(&workbench, cx);
        assert_eq!(chooser.read_with(cx, |d, _| d.title().to_owned()), "Select Target Changelist");
        chooser.update_in(cx, |d, window, cx| d.set_existing(false, window, cx));
        click_on(cx, "changelist-button-ok");
        wait_for_notice(&workbench, &format!("Undid commit {} “Add b”", short(&head)), cx);
        let lists = workbench.read_with(cx, |w, _| w.changelists().clone());
        let added = lists.lists.iter().find(|list| list.name == "Add b").expect("the list named after the commit");
        assert_eq!(lists.files.get("b.txt"), Some(&added.id), "the undone change is in the chosen list");
        assert_eq!(rev(&root, "HEAD"), first);
        assert_eq!(git_in(&root, &["diff", "--cached", "--name-only"]), "b.txt", "the changes are back, staged");
        assert_eq!(commit_message(&workbench, cx).trim(), "Add b\n\nWith a body");
    }

    /// Edit Commit Message… rewords the head commit and an older one without touching the files; the
    /// notification's Undo puts the last change back.
    #[gpui_kit::test]
    async fn edit_commit_message_rewords_the_head_and_an_older_commit_and_undo_restores_it(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "a.txt", "two\n", "two");
            commit_file(root, "a.txt", "three\n", "three");
        });
        let log = show_log(&workbench, 3, cx);
        let head = rev(&root, "HEAD");
        run_action(cx, "vcs.log.editMessage", Some(json!(head)));
        let dialog = wait_for_dialog::<MessageDialog>(&workbench, cx);
        let title = dialog.read_with(cx, |dialog, _| dialog.title().to_owned());
        assert_eq!(title, format!("Edit Commit Message ({})", short(&head)));
        assert_eq!(dialog.read_with(cx, |dialog, cx| dialog.message(cx)).trim(), "three");
        answer_message(&dialog, "three reworded", cx);
        wait_for_notice(&workbench, &format!("Commit message of {} changed", short(&head)), cx);
        assert_eq!(subjects_in(&root), ["three reworded", "two", "first"]);
        assert_eq!(read(&root, "a.txt"), "three\n");

        wait_long(cx, "the log follows", |cx| log_subjects(&log, cx).first().is_some_and(|s| s == "three reworded"));
        let older = rev(&root, "HEAD~1");
        run_action(cx, "vcs.log.editMessage", Some(json!(older)));
        let dialog = wait_for_dialog::<MessageDialog>(&workbench, cx);
        answer_message(&dialog, "two reworded", cx);
        let title = format!("Commit message of {} changed", short(&older));
        wait_for_notice(&workbench, &title, cx);
        assert_eq!(subjects_in(&root), ["three reworded", "two reworded", "first"]);

        let notice = notice_titled(&workbench, &title, cx).expect("the change is told");
        let labels: Vec<&str> = notice.actions.iter().map(|action| action.label.as_str()).collect();
        assert_eq!(labels, ["Undo"]);
        let undo = EntryAction::Own(notice.actions[0].clone());
        workbench.update_in(cx, |w, window, cx| w.run_notification_action(undo, window, cx));
        wait_for_notice(&workbench, "Commit message change undone", cx);
        assert_eq!(subjects_in(&root), ["three reworded", "two", "first"]);
    }

    /// Fixup… and Squash Into… prepare a `fixup!` / `squash!` message for the commit and show the
    /// Commit tool window.
    #[gpui_kit::test]
    async fn fixup_and_squash_into_prepare_the_commit_message(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "a.txt", "two\n", "two");
        });
        show_log(&workbench, 2, cx);
        let target = rev(&root, "HEAD");
        run_action(cx, "vcs.log.fixup", Some(json!(target)));
        assert_eq!(commit_message(&workbench, cx), "fixup! two");
        let fixed = workbench.update(cx, |w, cx| w.commit_state(cx).read(cx).fixup_target.clone());
        assert_eq!(fixed.map(|commit| commit.id), Some(target.clone()));
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(COMMIT_TOOL_WINDOW)));
        run_action(cx, "vcs.log.squashInto", Some(json!(target)));
        assert_eq!(commit_message(&workbench, cx), "squash! two");
    }

    /// Commit and Rebase after Fixup… on an older commit makes the fixup commit, then moves it into
    /// that commit: the history keeps its subjects and the older commit holds the change.
    #[gpui_kit::test]
    async fn commit_and_rebase_after_fixup_melds_the_change_into_the_older_commit(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "a.txt", "two\n", "two");
            commit_file(root, "b.txt", "b\n", "three");
        });
        show_log(&workbench, 3, cx);
        let target = rev(&root, "HEAD~1");
        std::fs::write(root.join("a.txt"), "two fixed\n").expect("the file is written");
        wait_long(cx, "the change is read", |cx| {
            workbench.read_with(cx, |w, _| w.vcs_state().is_some_and(|state| state.status.len() == 1))
        });
        run_action(cx, "vcs.log.fixup", Some(json!(target)));
        assert_eq!(commit_message(&workbench, cx), "fixup! two");
        let commit_box = workbench.read_with(cx, |w, _| w.commit_box().cloned()).expect("the commit box");
        wait_long(cx, "the box counts the change", |cx| commit_box.read_with(cx, |b, _| b.input().included == 1));
        commit_box.update(cx, |commit_box, cx| commit_box.commit(false, true, cx));
        wait_for_notice(&workbench, "Rebased", cx);
        assert_eq!(subjects_in(&root), ["three", "two", "first"]);
        assert_eq!(git_in(&root, &["show", "HEAD~1:a.txt"]), "two fixed");
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "");
    }

    /// Drop Commit asks and removes the commit with its changes; Drop Commits removes several, and
    /// "Don't ask again" turns the question off.
    #[gpui_kit::test]
    async fn drop_commit_removes_it_after_the_confirmation(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "b\n", "two");
            commit_file(root, "c.txt", "c\n", "three");
            commit_file(root, "d.txt", "d\n", "four");
            commit_file(root, "e.txt", "e\n", "five");
        });
        let log = show_log(&workbench, 5, cx);
        let three = rev(&root, "HEAD~2");
        run_action(cx, "vcs.log.dropCommit", Some(json!(three)));
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Drop Commit");
        cx.simulate_keystrokes("enter");
        wait_for_notice(&workbench, &format!("Dropped commit {}", short(&three)), cx);
        assert_eq!(subjects_in(&root), ["five", "four", "two", "first"]);
        assert!(!root.join("c.txt").exists() && root.join("d.txt").exists());

        wait_long(cx, "the log follows", |cx| log_subjects(&log, cx).len() == 4);
        let (five, two) = (rev(&root, "HEAD"), rev(&root, "HEAD~2"));
        run_action(cx, "vcs.log.dropCommit", Some(json!([five, two])));
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Drop Commits");
        let checkbox = cx.debug_bounds("prompt-checkbox").expect("the checkbox is drawn");
        cx.simulate_click(checkbox.center(), gpui_kit::Modifiers::default());
        cx.simulate_keystrokes("enter");
        wait_for_notice(&workbench, "Dropped 2 commits", cx);
        assert_eq!(subjects_in(&root), ["four", "first"]);
        let asks = cx.update(|_, cx| settings::effective("w1", cx).vcs.confirmation.confirm_drop_commits);
        assert!(!asks, "Don't ask again turns the question off");
    }

    /// Squash Commits… offers the selected commits' messages and melds them into one commit with the
    /// message typed.
    #[gpui_kit::test]
    async fn squash_commits_melds_the_selected_commits_with_the_typed_message(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "b\n", "two");
            commit_file(root, "c.txt", "c\n", "three");
            commit_file(root, "d.txt", "d\n", "four");
        });
        show_log(&workbench, 4, cx);
        let (three, two) = (rev(&root, "HEAD~1"), rev(&root, "HEAD~2"));
        run_action(cx, "vcs.log.squashCommits", Some(json!([three, two])));
        let dialog = wait_for_dialog::<MessageDialog>(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Squash 2 Commits");
        assert_eq!(dialog.read_with(cx, |dialog, cx| dialog.message(cx)), "two\n\nthree");
        answer_message(&dialog, "two and three", cx);
        wait_for_notice(&workbench, "Squashed 2 commits", cx);
        assert_eq!(subjects_in(&root), ["four", "two and three", "first"]);
        assert!(["b.txt", "c.txt", "d.txt"].iter().all(|file| root.join(file).exists()));
    }

    /// Interactively Rebase from Here… lists the commits from the chosen one to HEAD oldest first;
    /// a plan that rewords, drops, reorders and squashes gives that history.
    #[gpui_kit::test]
    async fn interactive_rebase_lists_the_commits_oldest_first_and_applies_the_plan(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "b\n", "two");
            commit_file(root, "c.txt", "c\n", "three");
            commit_file(root, "d.txt", "d\n", "four");
            commit_file(root, "e.txt", "e\n", "five");
        });
        show_log(&workbench, 5, cx);
        let (first, two) = (rev(&root, "HEAD~4"), rev(&root, "HEAD~3"));
        run_action(cx, "vcs.log.interactiveRebase", Some(json!(two)));
        let dialog = wait_for_dialog::<InteractiveRebaseDialog>(&workbench, cx);
        let title = dialog.update(cx, |dialog, cx| dialog.title(cx));
        assert_eq!(title, format!("Rebasing 4 commits onto {}", short(&first)));
        let texts: Vec<String> =
            dialog.read_with(cx, |dialog, _| dialog.rows().into_iter().map(|row| row.text).collect());
        assert_eq!(texts, ["two", "three", "four", "five"]);
        dialog.update(cx, |dialog, cx| {
            // five before four, four squashed into it, three dropped, two reworded.
            dialog.move_row(3, -1, cx);
            dialog.set_action(3, RebaseAction::Squash, cx);
            dialog.set_action(1, RebaseAction::Drop, cx);
            dialog.set_action(0, RebaseAction::Reword, cx);
            dialog.set_message(0, "two reworded", cx);
            dialog.press(crate::vcs::interactive_rebase_dialog::START, cx);
        });
        wait_for_notice(&workbench, "Rebase finished", cx);
        assert_eq!(subjects_in(&root), ["five", "two reworded", "first"]);
        assert!(!root.join("c.txt").exists(), "the dropped commit's file is gone");
        assert!(["b.txt", "d.txt", "e.txt"].iter().all(|file| root.join(file).exists()));
        // The notice is told again with Undo once HEAD after the rebase is read.
        wait_long(cx, "the Undo button", |cx| {
            notice_titled(&workbench, "Rebase finished", cx).is_some_and(|notice| !notice.actions.is_empty())
        });
        let notice = notice_titled(&workbench, "Rebase finished", cx).expect("the rebase is told");
        assert_eq!(notice.actions.iter().map(|action| action.label.as_str()).collect::<Vec<_>>(), ["Undo"]);
    }

    /// New Branch… and New Tag… at a commit create them there, a tag name in use is refused; Rebase
    /// Current Branch onto Selected Commit moves the branch, and Checkout Revision detaches HEAD at the
    /// commit.
    #[gpui_kit::test]
    async fn new_branch_new_tag_rebase_onto_and_checkout_at_a_commit(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "b\n", "two");
            commit_file(root, "c.txt", "c\n", "three");
            git_in(root, &["tag", "v1", "HEAD~2"]);
        });
        let log = show_log(&workbench, 3, cx);
        let (first, two, three) = (rev(&root, "HEAD~2"), rev(&root, "HEAD~1"), rev(&root, "HEAD"));

        run_action(cx, "vcs.log.newBranch", Some(json!(two)));
        let dialog = wait_for_prompt(&workbench, cx);
        retype(&dialog, "from-two", cx);
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the new branch", |cx| branch_label(&workbench, cx) == "from-two");
        assert_eq!(rev(&root, "from-two"), two);

        run_action(cx, "vcs.log.newTag", Some(json!(first)));
        let dialog = wait_for_dialog::<GitFormDialog>(&workbench, cx);
        let title = dialog.update(cx, |dialog, cx| dialog.title(cx));
        assert_eq!(title, format!("Create New Tag On {}", short(&first)));
        cx.simulate_input("v1");
        cx.run_until_parked();
        let rows = dialog.update(cx, |dialog, cx| dialog.spec(cx).rows);
        assert!(rows.contains(&crate::vcs::git_forms::FormRow::Error("Tag 'v1' already exists".to_owned())));
        cx.simulate_keystrokes("enter");
        assert!(workbench.read_with(cx, |w, _| w.git_form_dialog().is_some()), "a tag in use is refused");
        cx.simulate_keystrokes("backspace backspace");
        cx.simulate_input("v0");
        cx.simulate_keystrokes("enter");
        wait_for_notice(&workbench, &format!("Created tag v0 on {}", short(&first)), cx);
        assert_eq!(rev(&root, "v0^{commit}"), first);

        wait_long(cx, "the log follows", |cx| log_subjects(&log, cx).len() == 3);
        run_action(cx, "vcs.log.rebaseOntoCommit", Some(json!(three)));
        // Git moves the branch before it puts HEAD back on it: the rebase is over once it is told.
        wait_long(cx, "the rebase", |cx| {
            notification_titles(&workbench, cx).iter().any(|title| title.starts_with("Rebased from-two onto "))
        });
        assert_eq!(rev(&root, "from-two"), three);
        assert_eq!(git_in(&root, &["rev-parse", "--abbrev-ref", "HEAD"]), "from-two");

        let short_id = log.read_with(cx, |log, _| {
            log.commits().iter().find(|commit| commit.id == first).map(|commit| commit.short_id.clone())
        });
        let short_id = short_id.expect("the first commit is listed");
        run_action(cx, "vcs.log.checkoutRevision", Some(json!(first)));
        wait_for_notice(&workbench, &format!("Checked out {short_id} (detached HEAD)"), cx);
        assert_eq!(rev(&root, "HEAD"), first);
        assert_eq!(git_in(&root, &["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD", "HEAD is detached");
    }

    /// Copy Revision Number copies the hashes; Go to Parent and Go to Child move the selection; Go to
    /// Hash/Branch/Tag selects the commit of a typed tag and says when there is none; a commit bookmark
    /// selects its commit.
    #[gpui_kit::test]
    async fn copy_revision_and_the_navigation_actions_move_the_selection(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "b\n", "two");
            commit_file(root, "c.txt", "c\n", "three");
            git_in(root, &["tag", "v1", "HEAD~2"]);
        });
        let log = show_log(&workbench, 3, cx);
        let (first, two, three) = (rev(&root, "HEAD~2"), rev(&root, "HEAD~1"), rev(&root, "HEAD"));

        run_action(cx, "vcs.log.copyRevision", Some(json!([two, three])));
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied, Some(format!("{three} {two}")), "newest first, as listed");

        run_action(cx, "vcs.log.goToParent", Some(json!(three)));
        assert_eq!(selected(&log, cx), Some(two.clone()));
        run_action(cx, "vcs.log.goToChild", Some(json!(first)));
        assert_eq!(selected(&log, cx), Some(two.clone()));
        // Without an argument the selection is the commit.
        run_action(cx, "vcs.log.goToParent", None);
        assert_eq!(selected(&log, cx), Some(first.clone()));

        run_action(cx, "vcs.log.goToRef", None);
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Go to Hash/Branch/Tag");
        cx.simulate_input("main");
        cx.simulate_keystrokes("enter");
        wait_long(cx, "main is selected", |cx| selected(&log, cx).as_deref() == Some(three.as_str()));

        run_action(cx, "vcs.log.goToRef", None);
        wait_for_prompt(&workbench, cx);
        cx.simulate_input("v1");
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the tag's commit is selected", |cx| selected(&log, cx).as_deref() == Some(first.as_str()));

        run_action(cx, "vcs.log.goToRef", None);
        wait_for_prompt(&workbench, cx);
        cx.simulate_input("nowhere");
        cx.simulate_keystrokes("enter");
        wait_for_notice(&workbench, "'nowhere' was not found in the log", cx);

        // The bookmark names the repository as the window lists it.
        let repo = workbench.read_with(cx, |w, _| w.vcs_state().and_then(|state| state.roots.first().cloned()));
        let repo = repo.expect("the project is a repository");
        run_action(cx, "vcs.log.revealBookmark", Some(json!(format!("git-log:{repo}#{two}"))));
        wait_long(cx, "the bookmarked commit is selected", |cx| selected(&log, cx).as_deref() == Some(two.as_str()));
    }

    /// Compare with Local shows the commit against the working file; Compare Versions shows the older
    /// selected commit against the newer, and asks for two.
    #[gpui_kit::test]
    async fn compare_with_local_and_compare_versions_open_the_diff_window(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "a.txt", "two\n", "two");
        });
        show_log(&workbench, 2, cx);
        std::fs::write(root.join("a.txt"), "local\n").expect("the file is written");
        let (first, two) = (rev(&root, "HEAD~1"), rev(&root, "HEAD"));

        run_action(cx, "vcs.log.compareWithLocal", Some(json!(two)));
        wait_long(cx, "the diff", |cx| workbench.read_with(cx, |w, _| w.diff_dialog().is_some()));
        let data = workbench.read_with(cx, |w, cx| w.diff_dialog().map(|dialog| dialog.read(cx).data().clone()));
        let data = data.expect("the diff is shown");
        assert!(data.path.ends_with("a.txt"), "{}", data.path);
        assert!(matches!(&data.left, DiffSide::Rev { rev, .. } if *rev == two), "{:?}", data.left);
        assert_eq!(data.right, DiffSide::worktree());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        run_action(cx, "vcs.log.compareVersions", Some(json!([two, first])));
        wait_long(cx, "the diff of the versions", |cx| {
            workbench.read_with(cx, |w, cx| {
                w.diff_dialog().is_some_and(|dialog| matches!(&dialog.read(cx).data().right, DiffSide::Rev { .. }))
            })
        });
        let data = workbench.read_with(cx, |w, cx| w.diff_dialog().map(|dialog| dialog.read(cx).data().clone()));
        let data = data.expect("the diff is shown");
        assert!(matches!(&data.left, DiffSide::Rev { rev, .. } if *rev == first), "{:?}", data.left);
        assert!(matches!(&data.right, DiffSide::Rev { rev, .. } if *rev == two), "{:?}", data.right);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        run_action(cx, "vcs.log.compareVersions", Some(json!(two)));
        assert!(notification_titles(&workbench, cx).contains(&"Select two commits to compare".to_owned()));
    }

    /// View in Browser opens the commit's page on the remote's web site; without a remote it says
    /// there is no web URL.
    #[gpui_kit::test]
    async fn view_in_browser_opens_the_commit_page_of_the_remote(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["remote", "add", "origin", "git@github.com:octo/repo.git"]);
        });
        show_log(&workbench, 1, cx);
        let head = rev(&root, "HEAD");
        run_action(cx, "vcs.log.openInBrowser", Some(json!(head)));
        wait_long(cx, "the page opens", |cx| cx.opened_url().is_some());
        assert_eq!(cx.opened_url(), Some(format!("https://github.com/octo/repo/commit/{head}")));

        git_in(&root, &["remote", "remove", "origin"]);
        run_action(cx, "vcs.log.openInBrowser", Some(json!(head)));
        wait_for_notice(&workbench, "No web URL for remote 'origin'", cx);
    }

    /// Push All up to Here… opens the Push dialog of the current branch, which pushes the branch up to
    /// the chosen commit to the local bare repository `origin`; the commit after it is not pushed.
    #[gpui_kit::test]
    async fn push_all_up_to_here_pushes_the_branch_to_the_bare_remote(cx: &mut TestAppContext) {
        let remote = tempfile::tempdir().expect("a temporary folder");
        let parent = remote.path().canonicalize().expect("the folder exists");
        let bare = parent.join("origin.git");
        let bare_path = bare.to_string_lossy().into_owned();
        git_in(&parent, &["init", "-q", "--bare", "-b", "main", &bare_path]);
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["remote", "add", "origin", &bare_path]);
            git_in(root, &["push", "-q", "-u", "origin", "main"]);
            commit_file(root, "b.txt", "two\n", "second");
            commit_file(root, "c.txt", "three\n", "third");
        });
        show_log(&workbench, 3, cx);
        let second = rev(&root, "HEAD~1");
        run_action(cx, "vcs.log.pushUpToHere", Some(json!(second)));
        wait_long(cx, "the Push dialog", |cx| workbench.read_with(cx, |w, _| w.push_dialog().is_some()));
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the push", |_| git_in(&bare, &["rev-parse", "main"]) == second);
        assert_eq!(subjects_in(&root)[0], "third", "the commit after it stays local");
    }

    /// The changed files of a commit: Show Diff, Compare with Local and Compare Before with Local open
    /// the diff window, Jump to Source the file; Get from Revision restores the file's content from the
    /// commit; Revert Selected Changes takes the change back; Group by Directory changes the rows and
    /// Show Diff Preview is a toggle.
    #[gpui_kit::test]
    async fn the_changed_files_actions_act_on_the_files_of_the_selected_commit(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            std::fs::create_dir_all(root.join("src")).expect("the folder is made");
            std::fs::write(root.join("src/c.txt"), "new\n").expect("the file is written");
            commit_file(root, "a.txt", "two\n", "Second change");
        });
        let log = show_log(&workbench, 2, cx);
        let (first, second) = (rev(&root, "HEAD~1"), rev(&root, "HEAD"));
        log.update(cx, |log, cx| log.select(0, cx));
        let pane = workbench.read_with(cx, |w, _| w.log_details().cloned()).expect("the window has the details");
        wait_long(cx, "the changed files", |cx| pane.read_with(cx, |pane, _| pane.all_changes().len() == 2));
        std::fs::write(root.join("a.txt"), "local\n").expect("the file is written");

        let diff = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, cx| w.diff_dialog().map(|dialog| dialog.read(cx).data().clone()))
        };
        run_action(cx, "vcs.log.changes.showDiff", Some(json!(["a.txt"])));
        let shown = diff(cx).expect("the diff is shown");
        assert!(shown.path.ends_with("a.txt"));
        assert!(matches!(&shown.right, DiffSide::Rev { rev, .. } if *rev == second));
        cx.simulate_keystrokes("escape");
        run_action(cx, "vcs.log.changes.compareWithLocal", Some(json!(["a.txt"])));
        let shown = diff(cx).expect("the diff is shown");
        assert!(matches!(&shown.left, DiffSide::Rev { rev, .. } if *rev == second));
        assert_eq!(shown.right, DiffSide::worktree());
        cx.simulate_keystrokes("escape");
        run_action(cx, "vcs.log.changes.compareBeforeWithLocal", Some(json!(["a.txt"])));
        let shown = diff(cx).expect("the diff is shown");
        assert!(matches!(&shown.left, DiffSide::Rev { rev, .. } if *rev == first));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        run_action(cx, "vcs.log.changes.jumpToSource", Some(json!(["src/c.txt"])));
        wait_long(cx, "the file opens", |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("src/c.txt")))
        });

        run_action(cx, "vcs.log.changes.getFromRevision", Some(json!(["a.txt"])));
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Get from Revision");
        cx.simulate_keystrokes("enter");
        wait_for_notice(&workbench, "Got 1 file from revision", cx);
        assert_eq!(read(&root, "a.txt"), "two\n", "the file has its content from the commit");

        run_action(cx, "vcs.log.changes.revert", Some(json!(["a.txt"])));
        wait_for_notice(&workbench, "Changes reverted: 1 file changed", cx);
        assert_eq!(read(&root, "a.txt"), "one\n", "the commit's change to the file is taken back");
        assert!(root.join("src/c.txt").exists(), "the other file is left alone");

        let grouped = pane.read_with(cx, |pane, _| pane.lines());
        run_action(cx, GROUP_BY_DIRECTORY, None);
        let flipped = pane.read_with(cx, |pane, _| pane.lines());
        assert_ne!(grouped, flipped, "the rows follow the grouping");
        run_action(cx, GROUP_BY_DIRECTORY, None);
        assert_eq!(pane.read_with(cx, |pane, _| pane.lines()), grouped);

        let toggled = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let action = cx.global::<ActionRegistry>().get(DIFF_PREVIEW).expect("the action is registered");
                action_toggled(&action, window, cx)
            })
        };
        let before = toggled(cx).expect("Show Diff Preview is a toggle");
        run_action(cx, DIFF_PREVIEW, None);
        assert_eq!(toggled(cx), Some(!before));
        assert_eq!(cx.update(|_, cx| stored_log_options(cx).diff_preview), !before);
    }

    /// The Branches pane's actions: New Branch… from the selected branch, Mark As Favorite, Update
    /// Selected from a local bare remote, Navigate Log to Selected Branch and Delete Branch.
    #[gpui_kit::test]
    async fn the_branches_pane_actions_act_on_the_selected_branch(cx: &mut TestAppContext) {
        let remote = tempfile::tempdir().expect("a temporary folder");
        let parent = remote.path().canonicalize().expect("the folder exists");
        let bare = parent.join("origin.git");
        let bare_path = bare.to_string_lossy().into_owned();
        git_in(&parent, &["init", "-q", "--bare", "-b", "main", &bare_path]);
        let other = parent.join("other");
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["remote", "add", "origin", &bare_path]);
            git_in(root, &["push", "-q", "-u", "origin", "main"]);
            git_in(root, &["branch", "topic"]);
            git_in(root, &["push", "-q", "-u", "origin", "topic"]);
            git_in(root, &["checkout", "-q", "-b", "feature"]);
            commit_file(root, "f.txt", "f\n", "feature work");
            git_in(root, &["checkout", "-q", "main"]);
            git_in(root, &["branch", "old"]);
            git_in(&parent, &["clone", "-q", &bare_path, &other.to_string_lossy()]);
            git_in(&other, &["config", "user.name", "Bob"]);
            git_in(&other, &["config", "user.email", "bob@example.test"]);
            git_in(&other, &["checkout", "-q", "topic"]);
            commit_file(&other, "t.txt", "t\n", "topic upstream");
            git_in(&other, &["push", "-q", "origin", "topic"]);
            git_in(root, &["fetch", "-q", "origin"]);
        });
        let log = show_log(&workbench, 2, cx);
        let target = |name: &str, cx: &mut VisualTestContext| {
            let branch = workbench.read_with(cx, |w, _| {
                w.vcs_state().and_then(|state| state.branches.iter().find(|b| !b.remote && b.name == name).cloned())
            });
            serde_json::to_value(BranchTarget { ref_name: name.to_owned(), repo: None, branch }).expect("a target")
        };

        let arg = target("feature", cx);
        run_action(cx, "vcs.log.branches.navigate", Some(arg));
        let feature = rev(&root, "feature");
        wait_long(cx, "the branch's commit is selected", |cx| selected(&log, cx).as_deref() == Some(feature.as_str()));

        let arg = target("feature", cx);
        run_action(cx, "vcs.log.branches.toggleFavorite", Some(arg));
        let key = crate::vcs_ops::branch_ops::favorites_key(Some(&root.to_string_lossy()));
        let stored = cx.update(|_, cx| cx.global::<UiStorage>().get(&key).map(str::to_owned));
        assert!(stored.is_some_and(|names| names.contains("\"feature\"")), "feature is a favorite");

        let arg = target("topic", cx);
        run_action(cx, "vcs.log.branches.update", Some(arg));
        wait_long(cx, "the update", |_| rev(&root, "topic") == rev(&root, "origin/topic"));
        assert_eq!(git_in(&root, &["rev-parse", "--abbrev-ref", "HEAD"]), "main", "the current branch stays");

        let arg = json!([target("old", cx)]);
        run_action(cx, "vcs.log.branches.delete", Some(arg));
        wait_for_notice(&workbench, "Deleted branch old", cx);
        assert!(!git_in(&root, &["branch", "--format=%(refname:short)"]).lines().any(|name| name == "old"));

        let arg = target("feature", cx);
        run_action(cx, "vcs.log.branches.new", Some(arg));
        let dialog = wait_for_prompt(&workbench, cx);
        retype(&dialog, "feature-two", cx);
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the new branch", |cx| branch_label(&workbench, cx) == "feature-two");
        assert_eq!(rev(&root, "feature-two"), feature, "it starts where the selected branch is");
    }

    /// Collapse Linear Branches stores the option and gives it to the log, whose presented graph then
    /// leaves out the unreferenced commits of linear chains; Expand Linear Branches turns it off again.
    #[gpui_kit::test]
    async fn collapse_and_expand_linear_branches_set_the_option_the_log_presents_its_graph_with(
        cx: &mut TestAppContext,
    ) {
        let (workbench, cx, _folders, _root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["checkout", "-q", "-b", "feature"]);
            commit_file(root, "f.txt", "1\n", "feature one");
            commit_file(root, "f.txt", "2\n", "feature two");
            commit_file(root, "f.txt", "3\n", "feature three");
            git_in(root, &["checkout", "-q", "main"]);
            commit_file(root, "m.txt", "m\n", "on main");
            git_in(root, &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
        });
        let log = show_log(&workbench, 6, cx);
        let shown_rows = |cx: &mut VisualTestContext| {
            log.read_with(cx, |log, _| {
                let commits: Vec<RepoCommit> = log.commits().iter().cloned().map(RepoCommit::from).collect();
                let head: HashSet<String> =
                    log.commits().iter().filter(|c| c.refs.iter().any(|r| r == "HEAD")).map(|c| c.id.clone()).collect();
                present_log_graph(&commits, log.options(), &LogFragments::default(), &head).commits.len()
            })
        };
        assert_eq!(shown_rows(cx), 6);
        run_action(cx, COLLAPSE_LINEAR, None);
        assert!(log.read_with(cx, |log, _| log.options().collapse_linear));
        assert!(cx.update(|_, cx| stored_log_options(cx).collapse_linear), "the option is stored");
        // feature one, feature two and on main have one parent, one child and no reference.
        assert_eq!(shown_rows(cx), 3, "merge feature, feature three (its branch) and first stay");
        run_action(cx, EXPAND_LINEAR, None);
        assert!(!log.read_with(cx, |log, _| log.options().collapse_linear));
        assert_eq!(shown_rows(cx), 6);
    }

    /// The commit context menu of the selected commit lists IDEA's rows; Copy Commit Message copies
    /// the commit's message, and Copy Revision Number, an action row, runs on the commit.
    #[gpui_kit::test]
    async fn the_commit_context_menu_offers_the_commit_actions_and_runs_the_chosen_one(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "b\n", "Second\n\nWith a body");
        });
        let log = show_log(&workbench, 2, cx);
        log.update(cx, |log, cx| log.select(0, cx));
        let choose = |row: &str, cx: &mut VisualTestContext| {
            let menu = commit_menu_of(&workbench, cx);
            let lines = |cx: &mut VisualTestContext| menu.read_with(cx, |menu, _| menu.lines().remove(0));
            // The active row is drawn with a "> " marker.
            let listed = |line: &String| line.strip_prefix("> ").unwrap_or(line) == row;
            assert!(lines(cx).iter().any(listed), "{row} is in {:?}", lines(cx));
            for _ in 0..lines(cx).len() {
                if lines(cx).iter().any(|line| *line == format!("> {row}")) {
                    break;
                }
                cx.simulate_keystrokes("down");
            }
            cx.simulate_keystrokes("enter");
            cx.run_until_parked();
        };
        let rows = ["Copy Revision Number", "Cherry-Pick", "Reset Current Branch to Here…", "Undo Commit…"];
        for row in rows {
            let menu = commit_menu_of(&workbench, cx);
            let lines = menu.read_with(cx, |menu, _| menu.lines().remove(0));
            assert!(lines.iter().any(|line| line.strip_prefix("> ").unwrap_or(line) == row), "{row} is in {lines:?}");
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
        }
        choose("Copy Commit Message", cx);
        // A paged log may carry the subject only.
        let copied = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
        assert!(copied.starts_with("Second"), "{copied:?}");
        choose("Copy Revision Number", cx);
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some(rev(&root, "HEAD")));
    }

    /// The commit context menu of the log's selected commit, once it shows.
    fn commit_menu_of(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::ui::popup_menu::PopupMenu<LogCommand>> {
        let position = gpui_kit::point(px(40.0), px(40.0));
        workbench.update_in(cx, |w, window, cx| w.show_commit_menu(position, window, cx));
        wait_long(cx, "the commit menu", |cx| workbench.read_with(cx, |w, _| w.open_menu::<LogCommand>().is_some()));
        workbench.read_with(cx, |w, _| w.open_menu::<LogCommand>()).expect("the menu is open")
    }

    /// Whether the commit menu's row `label` is disabled.
    fn row_disabled(
        menu: &Entity<crate::ui::popup_menu::PopupMenu<LogCommand>>,
        label: &str,
        cx: &mut VisualTestContext,
    ) -> bool {
        let disabled =
            menu.read_with(cx, |menu, _| menu.rows().iter().find(|row| row.label == label).map(|row| row.disabled));
        disabled.unwrap_or_else(|| panic!("{label} is a row of the menu"))
    }

    /// A commit already on the branch's upstream (a local bare repository) has the history rewriting
    /// rows of its menu disabled, and Push All up to Here… too; a commit made since keeps them.
    #[gpui_kit::test]
    async fn the_commit_menu_disables_history_rewriting_for_a_pushed_commit(cx: &mut TestAppContext) {
        let remote = tempfile::tempdir().expect("a temporary folder");
        let parent = remote.path().canonicalize().expect("the folder exists");
        let bare = parent.join("origin.git");
        let bare_path = bare.to_string_lossy().into_owned();
        git_in(&parent, &["init", "-q", "--bare", "-b", "main", &bare_path]);
        let (workbench, cx, _folders, _root) = git_project(cx, |root| {
            init_repo(root);
            commit_file(root, "b.txt", "b\n", "pushed");
            git_in(root, &["remote", "add", "origin", &bare_path]);
            git_in(root, &["push", "-q", "-u", "origin", "main"]);
            commit_file(root, "c.txt", "c\n", "local");
        });
        let log = show_log(&workbench, 3, cx);
        let rewriting =
            ["Edit Commit Message…", "Fixup…", "Squash Into…", "Drop Commit", "Interactively Rebase from Here…"];

        // The newest commit is not on the upstream yet.
        log.update(cx, |log, cx| log.select(0, cx));
        let menu = commit_menu_of(&workbench, cx);
        for row in rewriting {
            assert!(!row_disabled(&menu, row, cx), "{row} is enabled for a local commit");
        }
        assert!(!row_disabled(&menu, "Push All up to Here…", cx));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // The one below it was pushed.
        log.update(cx, |log, cx| log.select(1, cx));
        let menu = commit_menu_of(&workbench, cx);
        for row in rewriting {
            assert!(row_disabled(&menu, row, cx), "{row} is disabled for a pushed commit");
        }
        assert!(row_disabled(&menu, "Push All up to Here…", cx), "it is on the upstream already");
        assert!(!row_disabled(&menu, "Reset Current Branch to Here…", cx), "a reset does not rewrite the upstream");
        cx.simulate_keystrokes("escape");
    }

    /// Create Patch… of two commits suggests a file in the project's root named after them, writes
    /// their changes oldest first, and the patch takes them back off in a clean clone (`--reverse`).
    #[gpui_kit::test]
    async fn create_patch_of_commits_writes_their_changes_oldest_first(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            // Author dates a day apart: the patch orders the commits by them.
            for (file, text, message, date) in [
                ("a.txt", "two\n", "second", "2026-01-01T10:00:00"),
                ("b.txt", "bee\n", "third", "2026-01-02T10:00:00"),
            ] {
                std::fs::write(root.join(file), text).expect("the file is written");
                git_in(root, &["add", "."]);
                git_in(root, &["commit", "-q", "--date", date, "-m", message]);
            }
        });
        let log = show_log(&workbench, 3, cx);
        let (second, third) = (rev(&root, "HEAD~1"), rev(&root, "HEAD"));
        run_action(cx, "vcs.log.createPatch", Some(json!([third, second])));
        let create = wait_for_dialog::<CreatePatchDialog>(&workbench, cx);
        let source = create.read_with(cx, |d, _| d.source().clone());
        assert_eq!(source, CreatePatchSource::Described("2 commits".to_owned()));
        assert_eq!(create.read_with(cx, |d, cx| d.file(cx)), format!("{}/2_commits.patch", root.to_string_lossy()));
        assert_eq!(create.read_with(cx, |d, _| d.reverse()), Some(false), "a commits' patch can be reversed");
        let out = tempfile::tempdir().expect("a temporary folder");
        let file = out.path().join("commits.patch");
        create.update(cx, |d, cx| {
            d.set_file(&file.to_string_lossy(), cx);
            d.press(crate::vcs::prompt_dialogs::OK, cx);
        });
        wait_for_notice(&workbench, "Patch saved to commits.patch", cx);
        // Settings › Version Control › Confirmation asks whether to show the file: no.
        wait_for_prompt(&workbench, cx);
        cx.simulate_keystrokes("escape");
        let patch = std::fs::read_to_string(&file).expect("the patch is written");
        let (a, b) = (patch.find("a/a.txt").expect("a.txt is in it"), patch.find("a/b.txt").expect("b.txt too"));
        assert!(a < b, "the older commit's change comes first: {patch}");
        let clones = tempfile::tempdir().expect("a temporary folder");
        let clone = clones.path().join("clone");
        git_in(clones.path(), &["clone", "-q", &root.to_string_lossy(), &clone.to_string_lossy()]);
        git_in(&clone, &["apply", "--check", "--reverse", &file.to_string_lossy()]);

        // One commit is described by its hash and subject, and named after its subject.
        run_action(cx, "vcs.log.createPatch", Some(json!(second)));
        let create = wait_for_dialog::<CreatePatchDialog>(&workbench, cx);
        let short_id = log.read_with(cx, |log, _| {
            log.commits().iter().find(|commit| commit.id == second).map(|commit| commit.short_id.clone())
        });
        let line = create.read_with(cx, |d, _| d.source().clone());
        let expected = format!("{} second", short_id.expect("the commit is listed"));
        assert_eq!(line, CreatePatchSource::Described(expected));
        assert_eq!(create.read_with(cx, |d, cx| d.file(cx)), format!("{}/second.patch", root.to_string_lossy()));
        create.update(cx, |d, cx| d.press(crate::vcs::prompt_dialogs::CANCEL, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.vcs_dialog_view::<CreatePatchDialog>().is_none()));
    }
}
