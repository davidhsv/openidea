//! The Git tool window: the project's log with the details of the selected commit beside it.
//!
//! The details pane asks for what it shows (the changed files, their line counts, the commit's
//! containing branches and signature); each is read on the backend, off the window's thread. A
//! newer request of a kind replaces the read in flight, and the pane drops an answer whose key is
//! no longer the one it asked under, so a slow answer for an earlier selection never shows. Show
//! Diff on a changed file opens the diff window with the file before and after the commit, and the
//! other changed files to move along.
//!
//! The filter bar above the log and the Branches pane beside it set the log's filters; the log is
//! read again with them. The pane's branch commands run as the Branches popup's do.

use super::branches::run_action_later;
use super::services_window::on_workbench;
use super::{Panel, Workbench};
use crate::backend::Backend;
use crate::platform::actions::ActionRegistry;
use crate::platform::i18n::Localization;
use crate::platform::settings;
use crate::platform::storage::UiStorage;
use crate::vcs::branches::Branch as SlimBranch;
use crate::vcs::branches_popup::{BranchesCommand, RepoTarget};
use crate::vcs::log_branches::{BranchesPaneEvent, BranchesPaneInput, LogBranchesPane, PaneRepository, SHOW_BRANCHES};
use crate::vcs::log_details::{
    ChangesCommand, ChangesLoad, ChangesPart, LineStatRange, LogDetailsEvent, LogDetailsInput, LogDetailsPane,
    PartFiles, changes_menu,
};
use crate::vcs::log_filters::{LogFilterBar, LogFilterEvent, LogFilterInput};
use crate::vcs::log_options::{LogOptions, STORAGE_KEY};
use crate::vcs::log_view::{GitLog, GitLogEvent};
use crate::vcs_more::log_changes::{ChangeSide, LogChange, ShownChanges, compare_change_with_local, show_changes_diff};
use crate::vcs_ops::log_store::{LogFiltersPatch, RepoCommit};
use crate::vcs_ops::ops::vcs_notice;
use crate::workbench::notifications::NotificationLevel;
use gpui_kit::{App, AppContext, ClipboardItem, Context, Entity, Focusable, Subscription, Task, Window};
use ide_core::IdeResult;
use ide_host::Host;
use ide_host::ops::vcs as git;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

/// The details pane of the Git tool window, with the reads it asked for.
pub(super) struct LogSlot {
    log: Entity<GitLog>,
    details: Entity<LogDetailsPane>,
    filter_bar: Entity<LogFilterBar>,
    branches: Entity<LogBranchesPane>,
    /// The repository's users, once the User filter asked for them.
    users: Vec<String>,
    users_read: Option<Task<()>>,
    /// The reads in flight: changed files, line counts, details. A newer one replaces each.
    changes: Option<Task<()>>,
    stats: Option<Task<()>>,
    detail: Option<Task<()>>,
    _events: [Subscription; 4],
}

fn repo_path(repo: Option<&str>) -> Option<PathBuf> {
    repo.map(PathBuf::from)
}

/// Reads the changed files of every part, in order; the first failure fails the load.
async fn read_parts(host: Arc<Host>, label: String, parts: Vec<ChangesPart>) -> IdeResult<Vec<PartFiles>> {
    let mut files = Vec::with_capacity(parts.len());
    for part in parts {
        files.push(match part {
            ChangesPart::Commit { repo, commit } => {
                PartFiles::Files(git::vcs_commit_files(&host, &label, repo_path(repo.as_deref()), commit.id).await?)
            }
            ChangesPart::Parent { repo, commit, index } => {
                let parent = commit.parents.get(index).cloned().unwrap_or_default();
                let repo = repo_path(repo.as_deref());
                PartFiles::Files(git::vcs_diff_revisions(&host, &label, repo, parent, commit.id).await?)
            }
            ChangesPart::Commits { repo, ids } => {
                PartFiles::Revisions(git::vcs_commits_changes(&host, &label, repo_path(repo.as_deref()), ids).await?)
            }
        });
    }
    Ok(files)
}

/// The numstat of every range; a range that cannot be read comes back empty.
async fn read_line_stats(
    host: Arc<Host>,
    label: String,
    ranges: Vec<LineStatRange>,
) -> IdeResult<Vec<(LineStatRange, Vec<ide_vcs::FileLineStats>)>> {
    let mut out = Vec::with_capacity(ranges.len());
    for range in ranges {
        let repo = repo_path(range.repo.as_deref());
        let read =
            git::vcs_diff_numstat(&host, &label, repo, range.from.clone(), range.to.clone(), range.paths.clone());
        let stats = read.await.unwrap_or_else(|error| {
            tracing::debug!("the line counts of {}..{} cannot be read: {}", range.from, range.to, error.message);
            Vec::new()
        });
        out.push((range, stats));
    }
    Ok(out)
}

pub(super) fn register_actions(registry: &mut ActionRegistry) {
    registry.handle(SHOW_BRANCHES, |_, window, cx| {
        on_workbench(window, cx, |workbench, _, cx| workbench.toggle_log_branches(cx))
    });
}

impl Workbench {
    /// Show Branches: the Branches pane of the Git log shows, or hides.
    fn toggle_log_branches(&mut self, cx: &mut Context<Self>) {
        let Some(slot) = &self.git_log else { return };
        let mut options = slot.log.read(cx).options().clone();
        options.show_branches = !options.show_branches;
        self.set_log_options(options, cx);
    }

    /// The Git tool window's view: the log, with the details pane beside it.
    pub(super) fn mount_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Panel {
        let label = self.label.clone();
        let log = cx.new(|cx| GitLog::new(&label, cx));
        let details = cx.new(LogDetailsPane::new);
        let filter_input = self.log_filter_input(&log, &[], cx);
        let filter_bar = cx.new(|cx| LogFilterBar::new(filter_input, window, cx));
        let branches = cx.new(LogBranchesPane::new);
        log.update(cx, |log, cx| {
            log.set_details(details.clone(), cx);
            log.set_filter_panes(filter_bar.clone(), branches.clone(), cx);
        });
        let filtered = cx.subscribe_in(&filter_bar, window, |workbench, _, event: &LogFilterEvent, window, cx| {
            workbench.log_filter_event(event, window, cx);
        });
        let chosen = cx.subscribe_in(&branches, window, |workbench, _, event: &BranchesPaneEvent, window, cx| {
            workbench.log_branches_event(event, window, cx);
        });
        let selected = cx.subscribe_in(&log, window, |workbench, _, event: &GitLogEvent, window, cx| match event {
            GitLogEvent::SelectionChanged => workbench.feed_log_details(cx),
            GitLogEvent::ShowMenu { position } => workbench.show_commit_menu(*position, window, cx),
        });
        let asked = cx.subscribe_in(&details, window, |workbench, _, event: &LogDetailsEvent, window, cx| {
            workbench.log_details_event(event, window, cx);
        });
        self.git_log = Some(LogSlot {
            log: log.clone(),
            details,
            filter_bar,
            branches,
            users: Vec::new(),
            users_read: None,
            changes: None,
            stats: None,
            detail: None,
            _events: [selected, asked, filtered, chosen],
        });
        self.sync_log_panes(cx);
        Panel::new(log, None)
    }

    /// The project goes: the Git tool window's view and its reads go with it.
    pub(super) fn drop_git_log(&mut self) {
        self.git_log = None;
    }

    /// Publishes `vcsLogFocus`: the keyboard is in the Git log or one of its panes.
    pub(super) fn publish_log_context(&self, window: &mut Window, cx: &mut App) {
        let focused = self.git_log.as_ref().is_some_and(|slot| {
            [
                slot.log.focus_handle(cx),
                slot.details.focus_handle(cx),
                slot.filter_bar.focus_handle(cx),
                slot.branches.focus_handle(cx),
            ]
            .iter()
            .any(|handle| handle.contains_focused(window, cx))
        });
        crate::platform::context::set_context(window, cx, "vcsLogFocus", focused);
    }

    /// The details pane of the Git tool window, once the window was shown.
    pub fn log_details(&self) -> Option<&Entity<LogDetailsPane>> {
        self.git_log.as_ref().map(|slot| &slot.details)
    }

    /// What the filter bar shows: the log's filters, the repository's branches and users.
    fn log_filter_input(&self, log: &Entity<GitLog>, users: &[String], cx: &Context<Self>) -> LogFilterInput {
        let vcs = self.vcs_state();
        let log = log.read(cx);
        LogFilterInput {
            filters: log.filters().clone(),
            branches: vcs.map_or_else(Vec::new, |vcs| {
                let slim = |branch: &ide_vcs::Branch| SlimBranch {
                    name: branch.name.clone(),
                    remote: branch.remote,
                    current: branch.current,
                };
                vcs.branches.iter().map(slim).collect()
            }),
            repositories: vcs.map_or_else(Vec::new, |vcs| vcs.roots.clone()),
            users: users.to_vec(),
            root: vcs.and_then(|vcs| vcs.root.clone()),
            branches_hidden: !log.options().show_branches,
            recent: Default::default(),
            bookmarks: Vec::new(),
        }
    }

    /// The filter bar and the Branches pane show the repository and the log's filters again.
    pub(super) fn sync_log_panes(&mut self, cx: &mut Context<Self>) {
        let Some(slot) = &self.git_log else { return };
        let (log, bar, pane) = (slot.log.clone(), slot.filter_bar.clone(), slot.branches.clone());
        let input = self.log_filter_input(&log, &slot.users, cx);
        bar.update(cx, |bar, cx| bar.set_input(input, cx));
        let (tags, favorites) = self.vcs_tags_and_favorites();
        let Some(vcs) = self.vcs_state() else { return };
        let filters = log.read(cx).filters().clone();
        let root = vcs.root.clone().unwrap_or_default();
        let input = BranchesPaneInput {
            repositories: vec![PaneRepository {
                name: vcs.repo_name(&root).to_owned(),
                root,
                branches: vcs.branches.clone(),
                tags,
            }],
            current: vcs.branch.as_ref().and_then(|branch| branch.head.clone()),
            heads: Default::default(),
            favorites,
            filter_branches: filters.branches,
            filter_repositories: filters.repositories.unwrap_or_default(),
            read_only: false,
        };
        pane.update(cx, |pane, cx| pane.set_input(input, cx));
    }

    /// The log reads again with `patch` applied to its filters.
    fn filter_log(&mut self, patch: LogFiltersPatch, cx: &mut Context<Self>) {
        let Some(slot) = &self.git_log else { return };
        slot.log.update(cx, |log, cx| log.set_filters(patch, cx));
        self.sync_log_panes(cx);
    }

    fn log_filter_event(&mut self, event: &LogFilterEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.clone() {
            LogFilterEvent::SetFilters(patch) => self.filter_log(*patch, cx),
            LogFilterEvent::SetBranches(branches) => {
                self.filter_log(LogFiltersPatch { branches: Some(branches), ..LogFiltersPatch::default() }, cx);
            }
            LogFilterEvent::LoadUsers => {
                // Every repository of the project names its authors; one that cannot be read names none.
                let roots: Vec<PathBuf> =
                    self.vcs_state().map(|vcs| vcs.roots.iter().map(PathBuf::from).collect()).unwrap_or_default();
                let task = self.log_read(
                    cx,
                    move |host, label| async move {
                        if roots.is_empty() {
                            return git::vcs_log_users(&host, &label, None).await;
                        }
                        let mut users = Vec::new();
                        for root in roots {
                            match git::vcs_log_users(&host, &label, Some(root)).await {
                                Ok(found) => users.extend(found),
                                Err(error) => tracing::debug!("a repository's users cannot be read: {}", error.message),
                            }
                        }
                        users.sort();
                        users.dedup();
                        Ok(users)
                    },
                    |workbench, users, cx| {
                        let users = users.unwrap_or_else(|error| {
                            tracing::warn!("the repository's users cannot be read: {error}");
                            Vec::new()
                        });
                        if let Some(slot) = workbench.git_log.as_mut() {
                            slot.users = users;
                        }
                        workbench.sync_log_panes(cx);
                    },
                );
                if let Some(slot) = self.git_log.as_mut() {
                    slot.users_read = task;
                }
            }
            LogFilterEvent::ShowBranches => self.toggle_log_branches(cx),
            LogFilterEvent::Refresh => {
                if let Some(slot) = &self.git_log {
                    slot.log.update(cx, |log, cx| log.reload(cx));
                }
            }
            LogFilterEvent::GoToRef => run_action_later("vcs.log.goToRef".to_owned(), None, window, cx),
            other => tracing::info!("{other:?} of the log's filter bar is not available"),
        }
    }

    fn log_branches_event(&mut self, event: &BranchesPaneEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.clone() {
            BranchesPaneEvent::SetLogFilter { branches, repositories } => {
                let repositories = Some(Some(repositories).filter(|repos| !repos.is_empty()));
                self.filter_log(LogFiltersPatch { branches: Some(branches), repositories, ..Default::default() }, cx);
            }
            BranchesPaneEvent::RunAction { id, arg } => run_action_later(id.to_owned(), arg, window, cx),
            BranchesPaneEvent::Branch { command, repo } => {
                let target = repo.map_or(RepoTarget::Selected, RepoTarget::Repo);
                self.run_branch_command(BranchesCommand { target, command }, window, cx);
            }
            BranchesPaneEvent::ShowMenu { position, items } => {
                let Some(pane) = self.git_log.as_ref().map(|slot| slot.branches.clone()) else { return };
                self.show_menu(
                    &items,
                    position,
                    move |_, command, _, cx| pane.update(cx, |pane, cx| pane.run_command(command, cx)),
                    window,
                    cx,
                );
            }
            BranchesPaneEvent::ShowTagMenu { tag, .. } => tracing::info!("tag {tag} has no menu here"),
            BranchesPaneEvent::Refresh => {
                self.refresh_vcs(cx);
                if let Some(slot) = &self.git_log {
                    slot.log.update(cx, |log, cx| log.reload(cx));
                }
            }
            // The pane keeps its selection for the actions it starts itself.
            BranchesPaneEvent::SelectionChanged(_) => {}
        }
    }

    /// The pane shows the log's selection.
    fn feed_log_details(&mut self, cx: &mut Context<Self>) {
        let Some(slot) = &self.git_log else { return };
        let (commit, options) = {
            let log = slot.log.read(cx);
            (log.selected().cloned(), log.options().clone())
        };
        let commits: Vec<RepoCommit> = commit
            .into_iter()
            .map(|commit| RepoCommit { commit, repo: None, ref_branches: Vec::new(), read_only: false })
            .collect();
        let input = LogDetailsInput {
            lead: commits.first().map(RepoCommit::key),
            commits,
            options,
            paths: Vec::new(),
            current_root: self.vcs_state().and_then(|vcs| vcs.root.clone()),
            read_only: false,
            non_modal: settings::effective(&self.label, cx).vcs.commit.non_modal,
        };
        slot.details.clone().update(cx, |pane, cx| pane.set_input(input, cx));
    }

    /// Runs `work` on the backend; `done` gets its answer on the window's thread, unless a newer
    /// read of the same kind replaced this one.
    fn log_read<T, F, W>(
        &self,
        cx: &mut Context<Self>,
        work: W,
        done: impl FnOnce(&mut Workbench, Result<T, String>, &mut Context<Workbench>) + 'static,
    ) -> Option<Task<()>>
    where
        T: Send + 'static,
        F: Future<Output = IdeResult<T>> + Send + 'static,
        W: FnOnce(Arc<Host>, String) -> F + Send + 'static,
    {
        let label = self.label.clone();
        let call = cx.try_global::<Backend>()?.with_host(move |host| work(host, label));
        Some(cx.spawn(async move |workbench, cx| {
            let answer = match call.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(error.message),
                Err(error) => Err(error.to_string()),
            };
            // The window is gone: there is nothing to show the answer in.
            let _ = workbench.update(cx, |workbench, cx| done(workbench, answer, cx));
        }))
    }

    fn log_details_event(&mut self, event: &LogDetailsEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(details) = self.log_details().cloned() else { return };
        match event.clone() {
            LogDetailsEvent::LoadChanges(ChangesLoad { key, parts }) => {
                let pane = details.clone();
                let task = self.log_read(
                    cx,
                    move |host, label| read_parts(host, label, parts),
                    move |_, files, cx| pane.update(cx, |pane, cx| pane.changes_loaded(&key, files, cx)),
                );
                if let Some(slot) = self.git_log.as_mut() {
                    slot.changes = task;
                }
            }
            LogDetailsEvent::LoadLineStats { key, ranges } => {
                let pane = details.clone();
                let task = self.log_read(
                    cx,
                    move |host, label| read_line_stats(host, label, ranges),
                    move |_, stats, cx| {
                        let stats = stats.unwrap_or_default();
                        pane.update(cx, |pane, cx| pane.line_stats_loaded(&key, &stats, cx));
                    },
                );
                if let Some(slot) = self.git_log.as_mut() {
                    slot.stats = task;
                }
            }
            LogDetailsEvent::LoadDetails { key, repo, revision } => {
                let pane = details.clone();
                let task = self.log_read(
                    cx,
                    move |host, label| async move {
                        git::vcs_log_details(&host, &label, repo_path(repo.as_deref()), revision).await
                    },
                    move |_, read, cx| pane.update(cx, |pane, cx| pane.details_loaded(&key, read, cx)),
                );
                if let Some(slot) = self.git_log.as_mut() {
                    slot.detail = task;
                }
            }
            LogDetailsEvent::ShowDiff { changes, all, preview } => {
                self.show_log_diff(&changes, &all, preview, window, cx)
            }
            LogDetailsEvent::Command { command, changes } => self.run_log_changes_command(command, changes, window, cx),
            LogDetailsEvent::ShowMenu { position, changes } => {
                let items = changes_menu(&changes);
                self.show_menu(
                    &items,
                    position,
                    move |workbench, command, window, cx| {
                        workbench.run_log_changes_command(command, changes.clone(), window, cx);
                    },
                    window,
                    cx,
                );
            }
            LogDetailsEvent::GoToRef { rev, .. } => {
                let Some(slot) = &self.git_log else { return };
                if !slot.log.update(cx, |log, cx| log.select_rev(&rev, cx)) {
                    tracing::info!("{rev} is not among the commits the log has read");
                }
            }
            LogDetailsEvent::CopyText(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            LogDetailsEvent::SetOptions(options) => self.set_log_options(*options, cx),
        }
    }

    /// The log's options changed in the pane: they are stored and the log shows them.
    pub(super) fn set_log_options(&mut self, options: LogOptions, cx: &mut Context<Self>) {
        match serde_json::to_string(&options) {
            Ok(json) => cx.default_global::<UiStorage>().set(STORAGE_KEY, json),
            Err(error) => tracing::warn!("cannot store the log options: {error}"),
        }
        if let Some(slot) = &self.git_log {
            slot.log.update(cx, |log, cx| log.set_options(options, cx));
        }
        self.feed_log_details(cx);
    }

    /// Show Diff of the log's changed files: the first of `changes`, with `all` to move along. A
    /// merge commit's file against both parents has no viewer here.
    fn show_log_diff(
        &mut self,
        changes: &[LogChange],
        all: &[LogChange],
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let non_modal = settings::effective(&self.label, cx).vcs.commit.non_modal;
        let root = self.vcs_state().and_then(|vcs| vcs.root.clone());
        match show_changes_diff(changes, all, preview, non_modal, root.as_deref()) {
            Some(ShownChanges::Diff { data, .. }) => self.open_diff(*data, window, cx),
            // The editor has no diff tabs: a selection's preview is not shown.
            Some(ShownChanges::PreviewTab(_)) => {}
            Some(ShownChanges::MergeWindow(_) | ShownChanges::MergeTab { .. }) => {
                let text = cx.default_global::<Localization>().0.tx("The diff of a merge commit cannot be shown");
                self.notify(vcs_notice(NotificationLevel::Warning, text), cx);
            }
            None => {}
        }
    }

    /// A row of the changed files' menu.
    pub(super) fn run_log_changes_command(
        &mut self,
        command: ChangesCommand,
        changes: Vec<LogChange>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = self.vcs_state().and_then(|vcs| vcs.root.clone());
        match command {
            ChangesCommand::ShowDiff => {
                let all = self.log_details().map(|pane| pane.read(cx).all_changes().to_vec()).unwrap_or_default();
                self.show_log_diff(&changes, &all, false, window, cx);
            }
            ChangesCommand::CompareWithLocal | ChangesCommand::CompareBeforeWithLocal => {
                let side = if matches!(command, ChangesCommand::CompareWithLocal) {
                    ChangeSide::After
                } else {
                    ChangeSide::Before
                };
                let data = changes.first().and_then(|change| compare_change_with_local(change, side, root.as_deref()));
                if let Some(data) = data {
                    self.open_diff(data, window, cx);
                }
            }
            ChangesCommand::JumpToSource => {
                let Some(change) = changes.first().filter(|change| change.after.is_some()) else { return };
                let Some(root) = change.repo.clone().or(root) else { return };
                self.open_project_file(PathBuf::from(root).join(&change.path), window, cx);
            }
            ChangesCommand::Revert => self.revert_log_changes(changes, window, cx),
            ChangesCommand::GetFromRevision => self.get_from_revision(changes, window, cx),
            ChangesCommand::ShowHistory { .. } => {
                tracing::info!("{command:?} on the log's changes is not available");
            }
        }
    }
}
