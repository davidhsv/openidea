//! The Commit tool window's changes: the files that differ from the last commit, grouped as
//! merge conflicts, changes and unversioned files, by directory, each in the colour of what
//! happened to it. Read from the backend when shown and again when the repository changes.
//! Enter or a double click shows a changed file's diff, F4 opens the file, and the context menu
//! offers both.
//!
//! In the Commit tool window each row has an inclusion checkbox, kept in the commit state the
//! window shares with the commit message box below the tree: a click on it or Space on the
//! selected row includes or leaves out the row's files. The box is drawn below the tree.

use super::changes::{
    ChangeKind, ChangeNode, ChangelistState, CheckState, EffectiveKind, FileStatus, NodeType, aggregate_check,
    build_change_tree, check_of, strip_slash, toggled_check,
};
use super::commit_box::CommitBox;
use crate::backend::{Backend, BackendEvent};
use crate::platform::i18n::Localization;
use crate::theme;
use crate::vcs_ops::changelists::with_list_defaults;
use crate::vcs_ops::commit_store::CommitState;
use crate::vcs_ops::git_tabs::ContentTab;
use gpui_kit::{
    AnyElement, AnyView, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, Render, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use ide_core::IdeEvent;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

/// What the changes view asks of its window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitPanelEvent {
    /// Open this changed file (Jump to Source).
    Open(PathBuf),
    /// Show the diff of this changed file.
    ShowDiff(PathBuf),
    /// Show the context menu of the selected file here.
    ShowMenu(gpui_kit::Point<gpui_kit::Pixels>),
    /// Show content tab `id` (Commit, Shelf).
    SelectTab(String),
    /// Move the files at `paths` to change list `list` (they were dropped on its group).
    MoveToChangelist { paths: Vec<String>, list: String },
}

/// One visible line: a group, a directory or a changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    key: String,
    depth: usize,
    node_type: NodeType,
    label: String,
    detail: Option<String>,
    kind: Option<EffectiveKind>,
    path: Option<String>,
    /// The files below a group or directory, or the file itself.
    files: Vec<FileStatus>,
    expanded: bool,
    changelist: Option<String>,
}

/// What the Commit tool window adds to the tree: the shared commit state, whose inclusion the
/// checkboxes show and change, and the message box drawn below.
struct CommitParts {
    state: Entity<CommitState>,
    commit_box: Entity<CommitBox>,
    _redraw: Subscription,
}

/// Changed files dragged onto a change list's group.
#[derive(Clone, Debug)]
pub struct DraggedChanges {
    pub paths: Vec<String>,
    label: String,
}

impl Render for DraggedChanges {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(2.0))
            .rounded(theme::length(cx, "radius-small"))
            .bg(theme::color(cx, "bg-popup"))
            .border_1()
            .border_color(theme::color(cx, "border-popup"))
            .text_color(theme::color(cx, "fg-strong"))
            .child(SharedString::from(self.label.clone()))
    }
}

pub struct CommitPanel {
    focus: FocusHandle,
    window_label: String,
    /// The project folder paths are shown relative to.
    root: String,
    status: Vec<FileStatus>,
    tree: Vec<ChangeNode>,
    /// Keys of the groups and directories the user closed; everything else is open.
    collapsed: BTreeSet<String>,
    rows: Rc<Vec<Row>>,
    selected: Option<String>,
    loading: bool,
    error: Option<String>,
    scroll: UniformListScrollHandle,
    listener: Option<u64>,
    reading: Option<Task<()>>,
    commit: Option<CommitParts>,
    /// The change lists the changes are grouped by.
    lists: Option<ChangelistState>,
    /// The content tabs above the tree, the chosen one, and what a tab other than Commit shows in
    /// place of the changes.
    tabs: Vec<ContentTab>,
    tab: String,
    content: Option<AnyView>,
    /// The window's own reads of the repository are shown here ([`Self::set_status`]), and the
    /// panel reads nothing itself: a read of its own would not wait for the window's Git command,
    /// and a status read writes the index while that command may need it.
    follows_window: bool,
    /// How many times the panel read the repository itself.
    #[cfg(test)]
    own_reads: usize,
}

impl EventEmitter<CommitPanelEvent> for CommitPanel {}

fn kind(kind: ide_vcs::ChangeKind) -> ChangeKind {
    match kind {
        ide_vcs::ChangeKind::Added => ChangeKind::Added,
        ide_vcs::ChangeKind::Modified => ChangeKind::Modified,
        ide_vcs::ChangeKind::Deleted => ChangeKind::Deleted,
        ide_vcs::ChangeKind::Renamed => ChangeKind::Renamed,
        ide_vcs::ChangeKind::Untracked => ChangeKind::Untracked,
        ide_vcs::ChangeKind::Ignored => ChangeKind::Ignored,
        ide_vcs::ChangeKind::Conflicted => ChangeKind::Conflicted,
    }
}

/// A changed file as the change models hold it.
pub(crate) fn file_status(status: ide_vcs::FileStatus) -> FileStatus {
    let ide_vcs::FileStatus { path, rel_path, staged, unstaged, renamed_from } = status;
    FileStatus {
        path: path.0.to_string_lossy().into_owned(),
        rel_path,
        staged: staged.map(kind),
        unstaged: unstaged.map(kind),
        renamed_from,
    }
}

/// The colour token of a change, by what happened to the file.
pub(super) fn kind_color(kind: EffectiveKind) -> &'static str {
    match kind {
        EffectiveKind::Modified | EffectiveKind::Renamed => "vcs-modified",
        EffectiveKind::Added => "vcs-added",
        EffectiveKind::Deleted => "vcs-deleted",
        EffectiveKind::Untracked => "vcs-untracked",
        EffectiveKind::Ignored => "vcs-ignored",
        EffectiveKind::Conflicted => "vcs-conflict",
    }
}

fn flatten(nodes: &[ChangeNode], depth: usize, collapsed: &BTreeSet<String>, rows: &mut Vec<Row>) {
    for node in nodes {
        let expanded = node.node_type != NodeType::File && !collapsed.contains(&node.key);
        rows.push(Row {
            key: node.key.clone(),
            depth,
            node_type: node.node_type,
            label: node.label.clone(),
            detail: node.detail.clone(),
            kind: node.kind,
            path: node.path.clone(),
            files: node.files.clone(),
            expanded,
            changelist: node.changelist.clone(),
        });
        if expanded {
            flatten(&node.children, depth + 1, collapsed, rows);
        }
    }
}

impl CommitPanel {
    /// A panel that reads the repository itself, now and on each change the backend reports.
    pub fn new(window_label: &str, root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::unread(window_label, root, cx);
        panel.read(cx);
        panel
    }

    /// A panel shown in a project window, which feeds it every read of the repository it makes
    /// ([`Self::set_status`]). `status`: what the window read last, shown while the panel is loading
    /// until [`Self::window_read_shown`] says a read made since has reached it; `None` when the
    /// project is no repository, and the panel then reads once itself.
    pub fn following_window(
        window_label: &str,
        root: PathBuf,
        status: Option<Vec<FileStatus>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::unread(window_label, root, cx);
        panel.follows_window = true;
        match status {
            Some(status) => {
                panel.status = status;
                panel.loading = true;
                panel.rebuild(cx);
            }
            None => panel.read(cx),
        }
        panel
    }

    /// A read of the window made after the panel was shown has reached it.
    pub fn window_read_shown(&mut self, cx: &mut Context<Self>) {
        self.loading = false;
        cx.notify();
    }

    fn unread(window_label: &str, root: PathBuf, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let listener = cx.try_global::<Backend>().is_some().then(|| {
            cx.global_mut::<Backend>().listen(move |event, cx| {
                let _ = weak.update(cx, |panel, cx| panel.backend_event(event, cx));
            })
        });
        cx.on_release(|panel, cx| {
            if let Some(listener) = panel.listener.filter(|_| cx.has_global::<Backend>()) {
                cx.global_mut::<Backend>().unlisten(listener);
            }
        })
        .detach();
        Self {
            focus: cx.focus_handle(),
            window_label: window_label.to_owned(),
            root: root.to_string_lossy().into_owned(),
            status: Vec::new(),
            tree: Vec::new(),
            collapsed: BTreeSet::new(),
            rows: Rc::default(),
            selected: None,
            loading: false,
            error: None,
            scroll: UniformListScrollHandle::new(),
            listener,
            reading: None,
            commit: None,
            lists: None,
            tabs: Vec::new(),
            tab: "changes".to_owned(),
            content: None,
            follows_window: false,
            #[cfg(test)]
            own_reads: 0,
        }
    }

    /// How many times the panel read the repository itself.
    #[cfg(test)]
    pub(crate) fn own_reads(&self) -> usize {
        self.own_reads
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The changed files as the backend last reported them.
    pub fn status(&self) -> &[FileStatus] {
        &self.status
    }

    /// The visible lines, indented by depth: a group or directory with its number of files.
    pub fn lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| {
                let count =
                    if row.node_type == NodeType::File { String::new() } else { format!(" ({})", row.files.len()) };
                format!("{}{}{count}", "  ".repeat(row.depth), row.label)
            })
            .collect()
    }

    /// Shows inclusion checkboxes kept in `state`, and `commit_box` below the tree.
    pub fn set_commit(&mut self, state: Entity<CommitState>, commit_box: Entity<CommitBox>, cx: &mut Context<Self>) {
        let redraw = cx.observe(&state, |_, _, cx| cx.notify());
        self.commit = Some(CommitParts { state, commit_box, _redraw: redraw });
        self.forget_gone_checks(cx);
        cx.notify();
    }

    /// The message box below the tree, in the Commit tool window.
    pub fn commit_box(&self) -> Option<&Entity<CommitBox>> {
        self.commit.as_ref().map(|parts| &parts.commit_box)
    }

    /// The labels of the changed files the next commit includes, fully or in part, in tree order.
    pub fn included_files(&self, cx: &App) -> Vec<String> {
        if self.commit.is_none() {
            return Vec::new();
        }
        let checks = &self.effective_checks(cx);
        self.rows
            .iter()
            .filter(|row| row.node_type == NodeType::File)
            .filter(|row| row.files.iter().any(|file| check_of(file, checks) != CheckState::None))
            .map(|row| row.label.clone())
            .collect()
    }

    /// Includes the files of rows `keys` in the next commit, or leaves them out: one file flips its
    /// inclusion; several are all left out when all are fully included, else all included.
    pub fn toggle_included(&mut self, keys: &[&str], cx: &mut Context<Self>) {
        let Some(parts) = &self.commit else { return };
        let mut files: Vec<FileStatus> = Vec::new();
        for row in self.rows.iter().filter(|row| keys.contains(&row.key.as_str())) {
            for file in &row.files {
                if !files.iter().any(|known| known.path == file.path) {
                    files.push(file.clone());
                }
            }
        }
        if files.is_empty() {
            return;
        }
        let current = self.effective_checks(cx);
        let total = aggregate_check(&files, &current);
        let target = if total.on == files.len() && !total.indeterminate { CheckState::None } else { CheckState::All };
        let mut next = current.clone();
        for file in &files {
            let check = if files.len() == 1 { toggled_check(check_of(file, &current)) } else { target };
            next.insert(file.path.clone(), check);
        }
        parts.state.update(cx, |state, cx| {
            state.checks = next;
            cx.notify();
        });
    }

    /// The inclusion the checkboxes show: what was chosen, and without a choice the changes of the
    /// active change list only.
    fn effective_checks(&self, cx: &App) -> HashMap<String, CheckState> {
        let Some(parts) = &self.commit else { return HashMap::new() };
        let checks = &parts.state.read(cx).checks;
        match &self.lists {
            Some(lists) => with_list_defaults(&self.status, &self.root, lists, checks),
            None => checks.clone(),
        }
    }

    /// Inclusion choices of files that are no longer changed are forgotten.
    fn forget_gone_checks(&mut self, cx: &mut Context<Self>) {
        let Some(parts) = &self.commit else { return };
        let live: HashSet<&str> = self.status.iter().map(|file| file.path.as_str()).collect();
        if parts.state.read(cx).checks.keys().all(|path| live.contains(path.as_str())) {
            return;
        }
        parts.state.update(cx, |state, cx| {
            state.checks.retain(|path, _| live.contains(path.as_str()));
            cx.notify();
        });
    }

    fn backend_event(&mut self, event: &BackendEvent, cx: &mut Context<Self>) {
        if event.window.as_deref().is_some_and(|window| window != self.window_label) {
            return;
        }
        if !self.follows_window && matches!(event.event, IdeEvent::VcsChanged { .. }) {
            self.read(cx);
        }
    }

    /// The project folder the changes are listed under.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// The key of the selected row.
    pub fn selected_key(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The path of the selected row, when it is a changed file.
    pub fn selected_file(&self) -> Option<String> {
        let key = self.selected.as_deref()?;
        let row = self.rows.iter().find(|row| row.key == key && row.node_type == NodeType::File)?;
        row.path.as_deref().map(|path| strip_slash(path).to_owned())
    }

    /// Whether the selected file can be opened: it is a changed file that was not deleted.
    pub fn selected_file_opens(&self) -> bool {
        let Some(key) = self.selected.as_deref() else { return false };
        self.rows
            .iter()
            .any(|row| row.key == key && row.node_type == NodeType::File && row.kind != Some(EffectiveKind::Deleted))
    }

    /// Groups the changes by `lists`.
    pub fn set_changelists(&mut self, lists: ChangelistState, cx: &mut Context<Self>) {
        if self.lists.as_ref() == Some(&lists) {
            return;
        }
        self.lists = Some(lists);
        self.rebuild(cx);
        cx.notify();
    }

    /// The change list of the selected row: a list's group or what is below it.
    pub fn selected_changelist(&self) -> Option<String> {
        let key = self.selected.as_deref()?;
        self.rows.iter().find(|row| row.key == key)?.changelist.clone()
    }

    /// The change list whose group row is selected.
    pub fn selected_list_group(&self) -> Option<String> {
        let key = self.selected.as_deref()?;
        self.rows.iter().find(|row| row.key == key && row.node_type == NodeType::Group)?.changelist.clone()
    }

    /// The paths, without a trailing slash, of the selected row's files.
    pub fn selected_paths(&self) -> Vec<String> {
        let Some(key) = self.selected.as_deref() else { return Vec::new() };
        let Some(row) = self.rows.iter().find(|row| row.key == key) else { return Vec::new() };
        row.files.iter().map(|file| strip_slash(&file.path).to_owned()).collect()
    }

    /// Shows `tabs` above the tree with `selected` chosen; `content` takes the place of the changes
    /// and the message box while a tab other than Commit is chosen.
    pub fn set_content_tabs(
        &mut self,
        tabs: Vec<ContentTab>,
        selected: &str,
        content: Option<AnyView>,
        cx: &mut Context<Self>,
    ) {
        let same = match (&self.content, &content) {
            (Some(shown), Some(new)) => shown.entity_id() == new.entity_id(),
            (None, None) => true,
            _ => false,
        };
        if same && self.tabs == tabs && self.tab == selected {
            return;
        }
        self.tabs = tabs;
        self.tab = selected.to_owned();
        self.content = content;
        cx.notify();
    }

    pub fn tabs(&self) -> &[ContentTab] {
        &self.tabs
    }

    pub fn selected_tab(&self) -> &str {
        &self.tab
    }

    /// What a tab other than Commit shows, while it is chosen.
    pub fn content(&self) -> Option<&AnyView> {
        self.content.as_ref()
    }

    /// Reads the changes again.
    pub fn read(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = cx.try_global::<Backend>() else { return };
        #[cfg(test)]
        {
            self.own_reads += 1;
        }
        let window = self.window_label.clone();
        let status = backend.with_host(move |host| async move { ide_host::ops::vcs::status(&host, &window).await });
        self.loading = true;
        self.reading = Some(cx.spawn(async move |panel, cx| {
            let status = status.await;
            let _ = panel.update(cx, |panel: &mut CommitPanel, cx| {
                panel.loading = false;
                match status {
                    Ok(Ok(status)) => {
                        panel.error = None;
                        panel.status = status.into_iter().map(file_status).collect();
                        panel.rebuild(cx);
                        panel.forget_gone_checks(cx);
                    }
                    Ok(Err(error)) => panel.error = Some(error.message),
                    Err(error) => panel.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Shows `status`, the window's own read of the repository, when it differs from what is shown:
    /// a change in the working tree alone brings no event that would make the view read again.
    pub fn set_status(&mut self, status: Vec<FileStatus>, cx: &mut Context<Self>) {
        // A read of the panel's own in flight answers for itself; a panel following its window
        // loads only through it.
        if (self.loading && !self.follows_window) || (self.error.is_none() && self.status == status) {
            return;
        }
        self.error = None;
        self.status = status;
        self.rebuild(cx);
        self.forget_gone_checks(cx);
        cx.notify();
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let i18n = &mut cx.default_global::<Localization>().0;
        self.tree = build_change_tree(&self.status, &self.root, true, self.lists.as_ref(), &[], false, i18n);
        self.flatten();
    }

    fn flatten(&mut self) {
        let mut rows = Vec::new();
        flatten(&self.tree, 0, &self.collapsed, &mut rows);
        if self.selected.as_ref().is_some_and(|key| !rows.iter().any(|row| &row.key == key)) {
            self.selected = None;
        }
        self.rows = Rc::new(rows);
    }

    /// Opens or closes the group or directory `key`.
    pub fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_owned());
        }
        self.flatten();
        cx.notify();
    }

    /// Shows the diff of row `key`'s file, or toggles its group or directory (Enter, double click).
    pub fn activate(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(row) = self.rows.iter().find(|row| row.key == key).cloned() else { return };
        match (row.node_type, row.path) {
            (NodeType::File, Some(path)) => cx.emit(CommitPanelEvent::ShowDiff(PathBuf::from(strip_slash(&path)))),
            (NodeType::File, None) => {}
            _ => self.toggle(key, cx),
        }
    }

    /// Opens the file of row `key` (F4, Jump to Source). A deleted file has nothing to open.
    pub fn open(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(row) = self.rows.iter().find(|row| row.key == key) else { return };
        if row.node_type == NodeType::File
            && row.kind != Some(EffectiveKind::Deleted)
            && let Some(path) = &row.path
        {
            cx.emit(CommitPanelEvent::Open(PathBuf::from(strip_slash(path))));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() || self.rows.is_empty() {
            return;
        }
        let rows = Rc::clone(&self.rows);
        let current = self.selected.as_ref().and_then(|key| rows.iter().position(|row| &row.key == key));
        let row = current.and_then(|index| rows.get(index));
        match (event.keystroke.key.as_str(), row) {
            ("down", _) => {
                self.selected = rows.get(current.map_or(0, |at| (at + 1).min(rows.len() - 1))).map(|r| r.key.clone())
            }
            ("up", _) => self.selected = rows.get(current.map_or(0, |at| at.saturating_sub(1))).map(|r| r.key.clone()),
            ("right", Some(row)) if row.node_type != NodeType::File && !row.expanded => {
                self.toggle(&row.key.clone(), cx)
            }
            ("left", Some(row)) if row.node_type != NodeType::File && row.expanded => self.toggle(&row.key.clone(), cx),
            ("enter", Some(row)) => self.activate(&row.key.clone(), cx),
            ("space", Some(row)) if self.commit.is_some() => self.toggle_included(&[row.key.as_str()], cx),
            ("f4", Some(row)) => self.open(&row.key.clone(), cx),
            _ => return,
        }
        if let Some(at) = self.selected.as_ref().and_then(|key| self.rows.iter().position(|row| &row.key == key)) {
            self.scroll.scroll_to_item(at, ScrollStrategy::Top);
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for CommitPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The inclusion checkbox of a row: ticked, mixed (some files or only some of their changes) or
/// empty.
fn include_mark(cx: &App, checked: bool, mixed: bool) -> Div {
    let on = checked || mixed;
    let mark = if mixed { Some("minus") } else { checked.then_some("check") };
    div()
        .flex_none()
        .size(px(14.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.0))
        .border_1()
        .border_color(theme::color(cx, if on { "bg-button-primary" } else { "border-checkbox" }))
        .bg(theme::color(cx, if on { "bg-button-primary" } else { "bg-checkbox" }))
        .children(mark.map(|icon| theme::icon(cx, icon, 10.0, theme::color(cx, "fg-button-primary"))))
}

impl CommitPanel {
    /// The tree, or what stands in its place while there is nothing to list.
    fn render_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.focus.is_focused(window);
        let (fg, muted) = (theme::color(cx, "fg-strong"), theme::color(cx, "fg-muted"));
        let message = |text: String| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(SharedString::from(text))
                .into_any_element()
        };
        if let Some(error) = &self.error {
            return message(error.clone());
        }
        if self.rows.is_empty() {
            let i18n = &cx.default_global::<Localization>().0;
            let text = if self.loading { i18n.tx("Loading…") } else { i18n.tx("No changes") };
            return message(text);
        }
        let selected_bg = theme::color(cx, if focused { "bg-tree-selected" } else { "bg-tree-selected-inactive" });
        let chevron_color = theme::color(cx, "fg-icon");
        let rows = Rc::clone(&self.rows);
        let selected = self.selected.clone();
        let checks = self.commit.as_ref().map(|_| self.effective_checks(cx));
        let drop_bg = theme::color(cx, "bg-tree-selected");
        uniform_list(
            "changes",
            rows.len(),
            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                rows[range]
                    .iter()
                    .map(|row| {
                        let folder = row.node_type != NodeType::File;
                        let color = row.kind.map_or(fg, |kind| theme::color(cx, kind_color(kind)));
                        let chevron = if folder {
                            theme::icon(
                                cx,
                                if row.expanded { "chevron-down" } else { "chevron-right" },
                                16.0,
                                chevron_color,
                            )
                        } else {
                            div().size(px(16.0)).into_any_element()
                        };
                        let icon = match row.node_type {
                            NodeType::File => {
                                Some(theme::icon(cx, theme::icons::file_icon_name(&row.label), 16.0, color))
                            }
                            NodeType::Dir => Some(theme::icon(cx, "folder", 16.0, fg)),
                            NodeType::Group => None,
                        };
                        let include = checks.as_ref().map(|checks| {
                            let total = aggregate_check(&row.files, checks);
                            let (key, label) = (row.key.clone(), row.label.clone());
                            include_mark(cx, total.checked, total.indeterminate)
                                .id(SharedString::from(format!("include:{}", row.key)))
                                .debug_selector(move || format!("commit-include-{label}"))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |panel, _: &MouseDownEvent, window, cx| {
                                        // The press is the checkbox's: the row is neither selected
                                        // nor activated, and the tree keeps the keyboard.
                                        window.prevent_default();
                                        cx.stop_propagation();
                                        window.focus(&panel.focus, cx);
                                        panel.toggle_included(&[key.as_str()], cx);
                                    }),
                                )
                        });
                        let (clicked, toggled, pressed) = (row.key.clone(), row.key.clone(), row.key.clone());
                        let row_label = row.label.clone();
                        let dragged =
                            (row.node_type != NodeType::Group && !row.files.is_empty()).then(|| DraggedChanges {
                                paths: row.files.iter().map(|file| strip_slash(&file.path).to_owned()).collect(),
                                label: row.label.clone(),
                            });
                        let target = row.changelist.clone().filter(|_| row.node_type == NodeType::Group);
                        div()
                            .id(SharedString::from(format!("change:{}", row.key)))
                            .debug_selector(move || format!("commit-row-{row_label}"))
                            .when_some(dragged, |el, dragged| {
                                el.on_drag(dragged, |dragged: &DraggedChanges, _, _, cx| cx.new(|_| dragged.clone()))
                            })
                            .when_some(target, |el, list| {
                                el.drag_over::<DraggedChanges>(move |style, _, _, _| style.bg(drop_bg)).on_drop(
                                    cx.listener(move |_, dragged: &DraggedChanges, _, cx| {
                                        let paths = dragged.paths.clone();
                                        cx.emit(CommitPanelEvent::MoveToChangelist { paths, list: list.clone() });
                                    }),
                                )
                            })
                            .h(theme::length(cx, "h-row"))
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .pl(px(4.0 + 20.0 * row.depth as f32))
                            .pr(px(8.0))
                            .whitespace_nowrap()
                            .text_color(color)
                            .when(selected.as_deref() == Some(row.key.as_str()), |el| el.bg(selected_bg))
                            .child(
                                div()
                                    .id(SharedString::from(format!("toggle:{}", row.key)))
                                    .size(px(16.0))
                                    .flex_none()
                                    .child(chevron)
                                    .when(folder, |el| {
                                        el.on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                            panel.toggle(&toggled, cx);
                                            cx.stop_propagation();
                                        }))
                                    }),
                            )
                            .children(include)
                            .children(icon)
                            .child(SharedString::from(row.label.clone()))
                            .children(
                                row.detail
                                    .clone()
                                    .map(|detail| div().text_color(muted).child(SharedString::from(detail))),
                            )
                            .when(folder, |el| {
                                let i18n = &mut cx.default_global::<Localization>().0;
                                let count = crate::vcs_diff::model::file_count(row.files.len(), i18n);
                                el.child(div().text_color(muted).child(SharedString::from(count)))
                            })
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |panel, event: &MouseDownEvent, window, cx| {
                                    window.focus(&panel.focus, cx);
                                    panel.selected = Some(pressed.clone());
                                    cx.emit(CommitPanelEvent::ShowMenu(event.position));
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            )
                            .on_click(cx.listener(move |panel, event: &ClickEvent, window, cx| {
                                window.focus(&panel.focus, cx);
                                panel.selected = Some(clicked.clone());
                                if event.click_count() >= 2 {
                                    panel.activate(&clicked, cx);
                                }
                                cx.notify();
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }
}

impl CommitPanel {
    /// The content tabs (Commit, Shelf), drawn as `commit-tab-<id>`.
    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let (fg, muted) = (theme::color(cx, "fg-strong"), theme::color(cx, "fg-muted"));
        let accent = theme::color(cx, "bg-button-primary");
        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .map(|tab| {
                let selected = tab.id == self.tab;
                let label = cx.default_global::<Localization>().0.tx(&tab.label);
                let label = match tab.count {
                    Some(count) => format!("{label} {count}"),
                    None => label,
                };
                let (id, selector) = (tab.id.clone(), tab.id.clone());
                div()
                    .id(SharedString::from(format!("commit-tab-{}", tab.id)))
                    .debug_selector(move || format!("commit-tab-{selector}"))
                    .h(px(26.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .text_color(if selected { fg } else { muted })
                    .when(selected, |el| el.border_b_1().border_color(accent))
                    .child(SharedString::from(label))
                    .on_click(
                        cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(CommitPanelEvent::SelectTab(id.clone()))),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .px(px(4.0))
            .border_b_1()
            .border_color(theme::color(cx, "border"))
            .children(tabs)
            .into_any_element()
    }
}

impl Render for CommitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strip = (self.tabs.len() > 1).then(|| self.render_tabs(cx));
        if let Some(content) = self.content.clone() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .text_size(theme::length(cx, "fs-ui"))
                .children(strip)
                .child(div().flex_1().min_h_0().w_full().child(content))
                .into_any_element();
        }
        let tree = self.render_tree(window, cx);
        // Only the tree takes the keyboard's keys: what is typed in the message box stays there.
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_size(theme::length(cx, "fs-ui"))
            .children(strip)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::key_down))
                    .child(tree),
            )
            .children(self.commit.as_ref().map(|parts| {
                div()
                    .flex_none()
                    .w_full()
                    .border_t_1()
                    .border_color(theme::color(cx, "border"))
                    .child(parts.commit_box.clone())
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AppDirs;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.test")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.test")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn settle(cx: &mut VisualTestContext, what: &str, done: impl Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..1000 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{what}: not within ten seconds");
    }

    fn lines(panel: &Entity<CommitPanel>, cx: &mut VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, _| panel.lines())
    }

    /// A real repository with a modified, a deleted and a new file: they are listed under
    /// Changes and Unversioned Files by directory, each with its kind, folders close and open,
    /// F4 asks for a file to be opened, Enter for its diff, and a change outside is picked up on the backend's event.
    #[gpui_kit::test]
    async fn lists_the_repositorys_changes_by_group_and_directory(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (home, project) = (tempfile::tempdir().expect("a home"), tempfile::tempdir().expect("a project"));
        let root = project.path().canonicalize().expect("the project folder exists");
        std::fs::create_dir(root.join("src")).expect("a folder");
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("the file is written");
        std::fs::write(root.join("src/gone.rs"), "// gone\n").expect("the file is written");
        std::fs::write(root.join("README.md"), "# readme\n").expect("the file is written");
        git(&root, &["init", "-q", "-b", "main"]);
        // The backend's git reads the machine's configuration: the untracked files are listed here.
        git(&root, &["config", "status.showUntrackedFiles", "normal"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "first"]);
        std::fs::write(root.join("src/main.rs"), "fn main() { println!(); }\n").expect("the file is written");
        std::fs::remove_file(root.join("src/gone.rs")).expect("removed");
        std::fs::write(root.join("notes.txt"), "new\n").expect("the file is written");
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            Backend::init(AppDirs::under(home.path()), cx).expect("the backend starts");
        });
        let open_root = root.clone();
        let opened = cx.update(|cx| {
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &open_root).await.map(|_| ())
            })
        });
        opened.await.expect("the task completes").expect("the project opens");

        let panel_root = root.clone();
        let (panel, cx) = cx.add_window_view(|_, cx| CommitPanel::new("w1", panel_root, cx));
        settle(cx, "the changes are read", |cx| panel.read_with(cx, |panel, _| !panel.is_loading()));
        assert_eq!(panel.read_with(cx, |panel, _| panel.error().map(str::to_owned)), None);
        assert_eq!(
            lines(&panel, cx),
            ["Changes (2)", "  src (2)", "    gone.rs", "    main.rs", "Unversioned Files (1)", "  notes.txt"]
        );
        panel.read_with(cx, |panel, _| {
            let kinds: Vec<(&str, Option<EffectiveKind>)> = panel
                .rows
                .iter()
                .filter(|row| row.node_type == NodeType::File)
                .map(|row| (row.label.as_str(), row.kind))
                .collect();
            assert_eq!(
                kinds,
                [
                    ("gone.rs", Some(EffectiveKind::Deleted)),
                    ("main.rs", Some(EffectiveKind::Modified)),
                    ("notes.txt", Some(EffectiveKind::Untracked)),
                ]
            );
        });

        let opened: Rc<std::cell::RefCell<Vec<PathBuf>>> = Rc::default();
        let shown: Rc<std::cell::RefCell<Vec<PathBuf>>> = Rc::default();
        let (log, shown_log) = (opened.clone(), shown.clone());
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&panel, move |_, event: &CommitPanelEvent, _| match event {
                CommitPanelEvent::Open(path) => log.borrow_mut().push(path.clone()),
                CommitPanelEvent::ShowDiff(path) => shown_log.borrow_mut().push(path.clone()),
                CommitPanelEvent::ShowMenu(_)
                | CommitPanelEvent::SelectTab(_)
                | CommitPanelEvent::MoveToChangelist { .. } => {}
            })
        });
        cx.update(|window, cx| {
            let focus = panel.read(cx).focus.clone();
            window.focus(&focus, cx);
        });
        // Down to Changes, to src; Left closes the folder, Right opens it again.
        cx.simulate_keystrokes("down down left");
        assert_eq!(lines(&panel, cx), ["Changes (2)", "  src (2)", "Unversioned Files (1)", "  notes.txt"]);
        cx.simulate_keystrokes("right down f4");
        assert!(opened.borrow().is_empty(), "a deleted file has nothing to open");
        cx.simulate_keystrokes("down f4");
        assert_eq!(*opened.borrow(), [root.join("src/main.rs")]);
        // Enter shows the diff of a changed file, a deleted one included.
        cx.simulate_keystrokes("enter up enter");
        assert_eq!(*shown.borrow(), [root.join("src/main.rs"), root.join("src/gone.rs")]);

        // The change is committed outside: the list follows on the backend's event.
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "second"]);
        let changed = BackendEvent {
            window: Some("w1".to_owned()),
            event: IdeEvent::VcsChanged { root: ide_core::IpcPath(root.clone()) },
        };
        panel.update(cx, |panel, cx| panel.backend_event(&changed, cx));
        settle(cx, "the list is empty", |cx| {
            lines(&panel, cx).is_empty() && panel.read_with(cx, |p, _| !p.is_loading())
        });
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    /// In the Commit tool window the checkboxes decide what the next commit includes: changed files
    /// start included and unversioned ones left out; Space on a folder leaves out all its files when
    /// all are in, and takes them all in again; a click on a file's checkbox flips only that file
    /// and neither selects the row nor opens its diff.
    #[gpui_kit::test]
    async fn the_checkboxes_include_and_leave_out_changes(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (home, project) = (tempfile::tempdir().expect("a home"), tempfile::tempdir().expect("a project"));
        let root = project.path().canonicalize().expect("the project folder exists");
        std::fs::create_dir(root.join("src")).expect("a folder");
        std::fs::write(root.join("src/a.rs"), "a\n").expect("the file is written");
        std::fs::write(root.join("src/b.rs"), "b\n").expect("the file is written");
        git(&root, &["init", "-q", "-b", "main"]);
        // The backend's git reads the machine's configuration: the untracked files are listed here.
        git(&root, &["config", "status.showUntrackedFiles", "normal"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "first"]);
        std::fs::write(root.join("src/a.rs"), "a2\n").expect("the file is written");
        std::fs::write(root.join("src/b.rs"), "b2\n").expect("the file is written");
        std::fs::write(root.join("notes.txt"), "new\n").expect("the file is written");
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            crate::platform::input::init(cx);
            Backend::init(AppDirs::under(home.path()), cx).expect("the backend starts");
        });
        let open_root = root.clone();
        let opened = cx.update(|cx| {
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &open_root).await.map(|_| ())
            })
        });
        opened.await.expect("the task completes").expect("the project opens");
        let panel_root = root.clone();
        let (panel, cx) = cx.add_window_view(|_, cx| CommitPanel::new("w1", panel_root, cx));
        let state = cx.update(|_, cx| cx.new(|_| CommitState::default()));
        panel.update_in(cx, |panel, window, cx| {
            let commit_box = cx.new(|cx| CommitBox::new(state.clone(), Default::default(), window, cx));
            panel.set_commit(state.clone(), commit_box, cx);
        });
        settle(cx, "the changes are read", |cx| lines(&panel, cx).len() == 6);
        let included = |cx: &mut VisualTestContext| panel.read_with(cx, |panel, cx| panel.included_files(cx));
        assert_eq!(included(cx), ["a.rs", "b.rs"]);
        let shown: Rc<std::cell::RefCell<Vec<CommitPanelEvent>>> = Rc::default();
        let log = shown.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&panel, move |_, event: &CommitPanelEvent, _| log.borrow_mut().push(event.clone()))
        });
        cx.update(|window, cx| {
            window.activate_window();
            let focus = panel.read(cx).focus.clone();
            window.focus(&focus, cx);
        });
        // Down to Changes, then src: Space leaves out both files, and takes them in again.
        cx.simulate_keystrokes("down down space");
        assert!(included(cx).is_empty());
        cx.simulate_keystrokes("space");
        assert_eq!(included(cx), ["a.rs", "b.rs"]);
        // A click on b.rs's checkbox leaves out that file only; the selection stays on src.
        for _ in 0..2 {
            cx.run_until_parked();
            let bounds = cx.debug_bounds("commit-include-b.rs").expect("the checkbox is drawn");
            cx.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
        }
        cx.run_until_parked();
        assert_eq!(included(cx), ["a.rs", "b.rs"], "two clicks flip it twice");
        let bounds = cx.debug_bounds("commit-include-b.rs").expect("the checkbox is drawn");
        cx.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(included(cx), ["a.rs"]);
        assert!(shown.borrow().is_empty(), "the checkbox opens no diff");
        assert!(panel.read_with(cx, |panel, _| panel.selected_key().is_some_and(|key| key.ends_with(":dir:src"))));
        // Space on the Unversioned Files group takes in the new file; the checks live in the shared state.
        cx.simulate_keystrokes("down down down space");
        assert_eq!(included(cx), ["a.rs", "notes.txt"]);
        let notes = root.join("notes.txt").to_string_lossy().into_owned();
        assert_eq!(state.read_with(cx, |state, _| state.checks.get(&notes).copied()), Some(CheckState::All));
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }
}
