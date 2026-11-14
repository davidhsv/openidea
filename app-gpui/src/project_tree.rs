//! The Project tool window's file tree: the project folder, listed lazily through the backend.
//!
//! [`crate::project_tree_model`] decides what the rows are (visibility, order, compacted folder
//! chains); this view owns what is loaded, expanded and selected, keeps the loaded folders in
//! step with the disk, and remembers the expansion and the view options between sessions.

use crate::backend::{Backend, BackendEvent};
use crate::platform::paths::is_same_or_inside;
use crate::platform::storage::UiStorage;
use crate::project_tree_model::{
    FsChange, Listings, ProjectNode, SortKey, TreeModel, TreeOptions, build_model, drop_listings, is_dir_entry,
    key_like, merge_listing, node_key_for, path_of_key, plan_refresh, visible_entries, visible_info,
};
use crate::project_tree_paths::ancestors_within;
use crate::project_view::store::ProjectViewState;
use crate::theme;
use gpui_kit::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, Pixels, Point, Render, ScrollStrategy, SharedString, Task, UniformListScrollHandle, Window, div,
    prelude::*, px, uniform_list,
};
use ide_core::error::ErrorKind;
use ide_core::ignored::IgnoredNames;
use ide_core::{IdeError, IdeEvent};
use ide_vfs::FileEntry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Where the view options are kept.
pub const OPTIONS_KEY: &str = "openidea.projectTree.options";

/// Where the expanded nodes of the project at `root` are kept.
pub fn expanded_key(root: &str) -> String {
    format!("openidea.projectTree.expanded:{root}")
}

/// No more expanded nodes than this are remembered.
const MAX_PERSISTED: usize = 3000;

/// How long the expansion rests before it is written, so a burst of toggles is one write.
const PERSIST_DELAY: Duration = Duration::from_millis(300);

/// The Project view's options as they are stored: what the tree shows and how the view behaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ViewOptions {
    pub show_unity_meta: bool,
    pub show_unity_generated: bool,
    pub show_hidden: bool,
    pub show_excluded: bool,
    pub compact: bool,
    pub folders_first: bool,
    /// `name`, `type` or `modified`; anything else counts as `name`.
    pub sort_by: String,
    /// Select the active editor's file in the tree.
    pub autoscroll_from_source: bool,
    /// Open the selected file while navigating the tree.
    pub autoscroll_to_source: bool,
    /// A single click opens a file.
    pub single_click_open: bool,
    /// A single click on a folder expands or collapses it.
    pub single_click_expand: bool,
    /// A single click shows a file in the preview tab.
    pub preview_tab: bool,
    /// A file's declarations show as rows below it.
    pub show_members: bool,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            show_unity_meta: false,
            show_unity_generated: false,
            show_hidden: false,
            show_excluded: true,
            compact: true,
            folders_first: true,
            sort_by: "name".to_owned(),
            autoscroll_from_source: false,
            autoscroll_to_source: false,
            single_click_open: false,
            single_click_expand: false,
            preview_tab: false,
            show_members: false,
        }
    }
}

impl ViewOptions {
    /// The stored options; the defaults for whatever is missing or unreadable.
    pub fn stored(storage: &UiStorage) -> Self {
        storage.get(OPTIONS_KEY).and_then(|text| serde_json::from_str(text).ok()).unwrap_or_default()
    }

    pub fn sort_key(&self) -> SortKey {
        match self.sort_by.as_str() {
            "type" => SortKey::Type,
            "modified" => SortKey::Modified,
            _ => SortKey::Name,
        }
    }

    /// What the model needs of these options.
    pub fn tree_options(&self) -> TreeOptions {
        TreeOptions {
            unity_root: None,
            show_unity_meta: self.show_unity_meta,
            show_unity_generated: self.show_unity_generated,
            show_hidden: self.show_hidden,
            show_excluded: self.show_excluded,
            compact: self.compact,
            folders_first: self.folders_first,
            sort_by: self.sort_key(),
        }
    }
}

/// One visible line of the tree.
#[derive(Debug, Clone)]
pub struct Row {
    pub node: Arc<ProjectNode>,
    pub depth: usize,
    pub expanded: bool,
}

/// The visible lines of `model`, top to bottom, each node once.
pub fn rows(model: &TreeModel, expanded: &BTreeSet<String>) -> Vec<Row> {
    fn walk<'a>(
        model: &'a TreeModel,
        expanded: &BTreeSet<String>,
        nodes: &'a [Arc<ProjectNode>],
        depth: usize,
        seen: &mut BTreeSet<&'a str>,
        rows: &mut Vec<Row>,
    ) {
        for node in nodes {
            let open = node.dir && expanded.contains(&node.key);
            if seen.insert(node.key.as_str()) {
                rows.push(Row { node: node.clone(), depth, expanded: open });
            }
            if let Some(kids) = model.children.get(&node.key).filter(|_| open) {
                walk(model, expanded, kids, depth + 1, seen, rows);
            }
        }
    }
    let mut out = Vec::new();
    walk(model, expanded, &model.roots, 0, &mut BTreeSet::new(), &mut out);
    out
}

/// The user asked to open a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFile(pub PathBuf);

/// The user asked for the context menu of the selection, at `position` in the window.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMenuRequested {
    pub position: Point<Pixels>,
}

/// The file tree of the project open in a window.
pub struct ProjectTree {
    focus: FocusHandle,
    window_label: String,
    root: String,
    name: String,
    listings: Listings,
    /// Keys of the expanded nodes.
    expanded: BTreeSet<String>,
    options: ViewOptions,
    ignored: IgnoredNames,
    model: TreeModel,
    /// Key of the selected node.
    selected: Option<String>,
    /// Folders being read.
    inflight: BTreeSet<String>,
    /// Folders asked to be read again while a read of theirs was under way: that read may have
    /// listed the folder before the change, so another one follows it.
    read_again: BTreeSet<String>,
    /// How many folder listings the tree asked the backend for.
    #[cfg(test)]
    folder_reads: usize,
    /// What reached the tree and what it read, with the time, for a test whose wait runs out.
    #[cfg(test)]
    trace: Vec<String>,
    /// A path to select and scroll to once its row exists.
    reveal: Option<String>,
    scroll: UniformListScrollHandle,
    listener: Option<u64>,
    persist: Option<Task<()>>,
}

impl EventEmitter<OpenFile> for ProjectTree {}
impl EventEmitter<ContextMenuRequested> for ProjectTree {}

impl ProjectTree {
    pub fn new(window_label: &str, root: PathBuf, cx: &mut Context<Self>) -> Self {
        let name = root.file_name().map_or_else(|| root.to_string_lossy(), |name| name.to_string_lossy()).into_owned();
        let root = root.to_string_lossy().into_owned();
        let storage = cx.try_global::<UiStorage>();
        let options = storage.map(ViewOptions::stored).unwrap_or_default();
        // The tree as it was left: a collapsed root stays collapsed. Only a first open expands it.
        let expanded = storage
            .and_then(|storage| storage.get(&expanded_key(&root)))
            .and_then(|text| serde_json::from_str::<Vec<String>>(text).ok())
            .map_or_else(|| BTreeSet::from([root.clone()]), BTreeSet::from_iter);
        let ignored = Self::ignored_names(window_label, cx);
        let weak = cx.weak_entity();
        let listener = cx.try_global::<Backend>().is_some().then(|| {
            cx.global_mut::<Backend>().listen(move |event, cx| {
                let _ = weak.update(cx, |tree, cx| tree.backend_event(event, cx));
            })
        });
        cx.on_release(|tree, cx| {
            if let Some(listener) = tree.listener.filter(|_| cx.has_global::<Backend>()) {
                cx.global_mut::<Backend>().unlisten(listener);
            }
            tree.write_expanded(cx);
        })
        .detach();
        let mut tree = Self {
            focus: cx.focus_handle(),
            window_label: window_label.to_owned(),
            root,
            name,
            listings: Listings::new(),
            expanded,
            options,
            ignored,
            model: TreeModel::default(),
            selected: None,
            inflight: BTreeSet::new(),
            read_again: BTreeSet::new(),
            #[cfg(test)]
            folder_reads: 0,
            #[cfg(test)]
            trace: Vec::new(),
            reveal: None,
            scroll: UniformListScrollHandle::new(),
            listener,
            persist: None,
        };
        tree.rebuild(cx);
        tree
    }

    /// `files.ignoredNames` as in effect for the window.
    fn ignored_names(window_label: &str, cx: &App) -> IgnoredNames {
        IgnoredNames::new(&crate::platform::settings::effective(window_label, cx).files.ignored_names)
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn model(&self) -> &TreeModel {
        &self.model
    }

    pub fn options(&self) -> &ViewOptions {
        &self.options
    }

    pub fn is_loaded(&self, dir: &str) -> bool {
        self.listings.contains_key(dir)
    }

    /// Copies what the tree shows into `state`, the model the file operations work on: the project,
    /// the options, what is loaded and expanded, and the selection.
    pub fn mirror_into(&self, state: &mut ProjectViewState) {
        state.root = Some(self.root.clone());
        state.name.clone_from(&self.name);
        state.options = self.options.clone();
        state.ignored = self.ignored.clone();
        state.listings.clone_from(&self.listings);
        state.expanded.clone_from(&self.expanded);
        let selected = self.selected.clone();
        state.set_selection(selected.clone(), selected);
    }

    /// Takes over what an operation changed in `state`: the expansion, and the row it asked to
    /// reveal. Returns whether the tree is to get the keyboard focus for that row.
    pub fn follow_state(&mut self, state: &mut ProjectViewState, cx: &mut Context<Self>) -> Option<bool> {
        if state.expanded != self.expanded {
            self.expanded.clone_from(&state.expanded);
            self.expansion_changed(cx);
        }
        let request = state.reveal_request.take()?;
        self.reveal_path(&request.path, cx);
        Some(request.focus)
    }

    /// A read of the folder `path`, made elsewhere, as if the tree had read it itself.
    pub fn put_read(&mut self, path: &str, read: Result<Vec<FileEntry>, IdeError>, cx: &mut Context<Self>) {
        match read {
            Ok(entries) => self.listed(path, entries, cx),
            Err(error) if matches!(error.kind, ErrorKind::NotFound | ErrorKind::InvalidInput) => {
                self.folder_gone(path, cx);
            }
            Err(error) => {
                tracing::warn!("project view: cannot read {path}: {}", error.message);
                self.listed_unreadable(path, cx);
            }
        }
    }

    /// The visible lines, top to bottom.
    pub fn rows(&self) -> Vec<Row> {
        rows(&self.model, &self.expanded)
    }

    /// Key of the selected node.
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Path of the selected file or folder.
    pub fn selected_path(&self) -> Option<&str> {
        let node = self.model.by_key.get(self.selected.as_deref()?)?;
        (!node.group && node.member.is_none() && node.placeholder.is_none()).then_some(node.path.as_str())
    }

    /// The folder the selection stands for: a selected folder itself, the folder of a selected file.
    pub fn selected_directory(&self) -> Option<String> {
        let node = self.model.by_key.get(self.selected.as_deref()?)?;
        if node.group || node.placeholder.is_some() {
            return None;
        }
        if node.dir {
            return Some(node.path.clone());
        }
        std::path::Path::new(&node.path).parent().map(|parent| parent.to_string_lossy().into_owned())
    }

    /// Rebuilds the rows from what is loaded, asks for the folders that are open but not read,
    /// and serves a pending reveal.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.model =
            build_model(&self.root, &self.name, &self.listings, &self.options.tree_options(), &self.ignored, None);
        for dir in visible_info(&self.model, &self.expanded).pending {
            self.load_dir(&dir, false, cx);
        }
        self.serve_reveal();
        cx.notify();
    }

    /// Reads one folder, or re-reads it with `force`. A folder being read is not asked for twice.
    pub fn load_dir(&mut self, path: &str, force: bool, cx: &mut Context<Self>) {
        if (!force && self.listings.contains_key(path)) || !cx.has_global::<Backend>() {
            return;
        }
        if !self.inflight.insert(path.to_owned()) {
            if force {
                self.read_again.insert(path.to_owned());
            }
            #[cfg(test)]
            self.trace_line(format!("read of {path} asked while one is in flight; again after it: {force}"));
            return;
        }
        #[cfg(test)]
        {
            self.folder_reads += 1;
            self.trace_line(format!("read of {path} starts"));
        }
        let (window, dir, path) = (self.window_label.clone(), PathBuf::from(path), path.to_owned());
        let listing = cx
            .global::<Backend>()
            .with_host(move |host| async move { ide_host::ops::fs::read_dir(&host, &window, dir).await });
        cx.spawn(async move |tree, cx| {
            let listing = listing.await;
            let _ = tree.update(cx, |tree: &mut ProjectTree, cx| {
                tree.inflight.remove(&path);
                let read = listing.unwrap_or_else(|error| Err(IdeError::internal(format!("{error:#}"))));
                #[cfg(test)]
                tree.trace_line(match &read {
                    Ok(entries) => format!("read of {path} done: {} entries", entries.len()),
                    Err(error) => format!("read of {path} failed: {}", error.message),
                });
                tree.put_read(&path, read, cx);
                if tree.read_again.remove(&path) {
                    tree.load_dir(&path, true, cx);
                }
            });
        })
        .detach();
    }

    fn put_listing(&mut self, path: &str, entries: Vec<FileEntry>) -> bool {
        match merge_listing(self.listings.get(path).map(Vec::as_slice), entries) {
            Some(merged) => {
                self.listings.insert(path.to_owned(), merged);
                true
            }
            None => false,
        }
    }

    fn listed(&mut self, path: &str, entries: Vec<FileEntry>, cx: &mut Context<Self>) {
        let changed = self.put_listing(path, entries);
        // Compacting a chain of single folders needs the listing one level down.
        let options = self.options.tree_options();
        if options.compact
            && let Some(listing) = self.listings.get(path)
            && let &[only] = visible_entries(listing, &options, &self.ignored).as_slice()
            && is_dir_entry(only)
            && !only.ignored
        {
            let below = only.path.to_string_lossy().into_owned();
            self.load_dir(&below, false, cx);
        }
        if changed {
            self.rebuild(cx);
        }
    }

    /// A folder that cannot be read still counts as loaded, with what was last known of it.
    fn listed_unreadable(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.listings.contains_key(path) {
            self.listings.insert(path.to_owned(), Vec::new());
            self.rebuild(cx);
        }
    }

    /// The folder no longer exists: it and everything loaded or expanded below it goes.
    fn folder_gone(&mut self, path: &str, cx: &mut Context<Self>) {
        drop_listings(&mut self.listings, &[path.to_owned()]);
        let root = self.root.clone();
        self.expanded.retain(|key| !is_same_or_inside(path_of_key(key), path) || *key == root);
        if path == root {
            self.listings.insert(root, Vec::new());
        }
        self.rebuild(cx);
    }

    fn backend_event(&mut self, event: &BackendEvent, cx: &mut Context<Self>) {
        if event.window.as_deref().is_some_and(|window| window != self.window_label) {
            return;
        }
        let paths = |paths: &[ide_core::IpcPath]| -> Vec<String> {
            paths.iter().map(|path| path.0.to_string_lossy().into_owned()).collect()
        };
        match &event.event {
            IdeEvent::FsChanged { created, modified, removed } => {
                #[cfg(test)]
                self.trace_line(format!("event fs created={created:?} modified={modified:?} removed={removed:?}"));
                self.apply_fs_change(&paths(created), &paths(modified), &paths(removed), cx);
            }
            // Ignored Files and Folders is read live: the rows follow the setting.
            IdeEvent::SettingsChanged { key } if key.is_empty() || key == "files" || key.starts_with("files.") => {
                self.ignored = Self::ignored_names(&self.window_label, cx);
                self.rebuild(cx);
            }
            IdeEvent::ExcludedDirsChanged { dirs } => {
                for dir in paths(dirs) {
                    if self.listings.contains_key(&dir) {
                        self.load_dir(&dir, true, cx);
                    }
                }
            }
            _ => {}
        }
    }

    /// A change on disk: re-reads the loaded folders it touches, and forgets the ones it removed.
    pub fn apply_fs_change(
        &mut self,
        created: &[String],
        modified: &[String],
        removed: &[String],
        cx: &mut Context<Self>,
    ) {
        let loaded: BTreeSet<String> = self.listings.keys().cloned().collect();
        let plan = plan_refresh(&FsChange { created, modified, removed }, &loaded);
        let gone = |key: &str| {
            let path = path_of_key(key);
            plan.drop.iter().any(|dropped| is_same_or_inside(path, dropped)) || removed.iter().any(|at| at == path)
        };
        if self.selected.as_deref().is_some_and(gone) {
            self.selected = None;
        }
        if !plan.drop.is_empty() {
            drop_listings(&mut self.listings, &plan.drop);
            self.rebuild(cx);
        }
        for dir in &plan.reload {
            self.load_dir(dir, true, cx);
        }
        cx.notify();
    }

    /// How many folder listings the tree asked the backend for.
    #[cfg(test)]
    pub(crate) fn folder_reads(&self) -> usize {
        self.folder_reads
    }

    #[cfg(test)]
    fn trace_line(&mut self, line: String) {
        self.trace.push(format!("{:.0} {line}", crate::platform::local_time::now_ms()));
    }

    /// The last lines of what reached the tree and what it read.
    #[cfg(test)]
    pub(crate) fn trace_report(&self) -> String {
        self.trace.iter().rev().take(30).rev().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
    }

    /// Re-reads every loaded folder (View › Reload from Disk).
    pub fn reload_all(&mut self, cx: &mut Context<Self>) {
        for dir in self.listings.keys().cloned().collect::<Vec<_>>() {
            self.load_dir(&dir, true, cx);
        }
    }

    /// Expands or collapses the folder node `key`. Expanding a compacted chain opens every
    /// folder in it, so the rows stay open should the chain split.
    pub fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(node) = self.model.by_key.get(key).filter(|node| node.dir).cloned() else { return };
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
            self.expanded.extend(node.chain.iter().map(|folder| key_like(key, folder)));
        }
        self.expansion_changed(cx);
    }

    fn expansion_changed(&mut self, cx: &mut Context<Self>) {
        for dir in visible_info(&self.model, &self.expanded).pending {
            self.load_dir(&dir, false, cx);
        }
        self.persist = Some(cx.spawn(async move |tree, cx| {
            cx.background_executor().timer(PERSIST_DELAY).await;
            let _ = tree.update(cx, |tree: &mut ProjectTree, cx| tree.write_expanded(cx));
        }));
        cx.notify();
    }

    fn write_expanded(&self, cx: &mut App) {
        if !cx.has_global::<UiStorage>() {
            return;
        }
        let keys: Vec<&String> = self.expanded.iter().take(MAX_PERSISTED).collect();
        match serde_json::to_string(&keys) {
            Ok(text) => cx.global_mut::<UiStorage>().set(&expanded_key(&self.root), text),
            Err(error) => tracing::warn!("project view: cannot store the expansion: {error}"),
        }
    }

    /// Changes the view options, for every Project view from now on.
    pub fn set_options(&mut self, change: impl FnOnce(&mut ViewOptions), cx: &mut Context<Self>) {
        let before = self.options.clone();
        change(&mut self.options);
        if self.options == before {
            return;
        }
        if cx.has_global::<UiStorage>() {
            match serde_json::to_string(&self.options) {
                Ok(text) => cx.global_mut::<UiStorage>().set(OPTIONS_KEY, text),
                Err(error) => tracing::warn!("project view: cannot store the options: {error}"),
            }
        }
        self.rebuild(cx);
    }

    pub fn select(&mut self, key: &str, cx: &mut Context<Self>) {
        self.selected = Some(key.to_owned());
        cx.notify();
    }

    /// Opens the file of the node `key`, or toggles its folder (Enter, double click).
    pub fn activate(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(node) = self.model.by_key.get(key).cloned() else { return };
        if node.dir {
            self.toggle(key, cx);
        } else if node.entry.is_some() {
            cx.emit(OpenFile(PathBuf::from(&node.path)));
        }
    }

    /// Expands the folders above `path` and selects it once its row exists. False when the
    /// path is outside the project.
    pub fn reveal_path(&mut self, path: &str, cx: &mut Context<Self>) -> bool {
        if !is_same_or_inside(path, &self.root) {
            return false;
        }
        let folders: Vec<String> = ancestors_within(&self.root, path).into_iter().map(str::to_owned).collect();
        self.expanded.extend(folders.iter().cloned());
        self.reveal = Some(path.to_owned());
        for folder in &folders {
            self.load_dir(folder, false, cx);
        }
        self.expansion_changed(cx);
        self.serve_reveal();
        true
    }

    /// Selects the revealed path if its row is there: the node showing exactly that path.
    fn serve_reveal(&mut self) {
        let Some(path) = self.reveal.as_deref() else { return };
        let Some(key) = node_key_for(&self.model, path).map(str::to_owned) else { return };
        let shows_it = self.model.by_key.get(&key).is_some_and(|node| node.chain.iter().any(|at| at == path));
        if !shows_it {
            return;
        }
        // A compacted chain may be keyed by a folder that was not expanded by path.
        if let Some(index) = self.rows().iter().position(|row| row.node.key == key) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Center);
            self.selected = Some(key);
            self.reveal = None;
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.shift {
            return;
        }
        let rows = self.rows();
        let current = self.selected.as_ref().and_then(|selected| rows.iter().position(|row| &row.node.key == selected));
        let row = current.and_then(|index| rows.get(index));
        let key_at = |index: usize| rows.get(index).map(|row| row.node.key.clone());
        match (event.keystroke.key.as_str(), row) {
            ("down", _) => self.selected = key_at(current.map_or(0, |index| (index + 1).min(rows.len() - 1))),
            ("up", _) => self.selected = key_at(current.map_or(0, |index| index.saturating_sub(1))),
            // Right opens a closed folder, then steps into it; Left closes an open folder, else
            // goes to the parent.
            ("right", Some(row)) if row.node.dir && !row.expanded => self.toggle(&row.node.key.clone(), cx),
            ("right", Some(row)) if row.node.dir => self.selected = current.and_then(|index| key_at(index + 1)),
            ("left", Some(row)) if row.node.dir && row.expanded => self.toggle(&row.node.key.clone(), cx),
            ("left", Some(row)) => {
                let parent =
                    current.and_then(|index| rows[..index].iter().rev().find(|candidate| candidate.depth < row.depth));
                if let Some(parent) = parent {
                    self.selected = Some(parent.node.key.clone());
                }
            }
            ("enter", Some(row)) => self.activate(&row.node.key.clone(), cx),
            _ => return,
        }
        if let Some(index) = self.selected.as_ref().and_then(|key| rows.iter().position(|row| &row.node.key == key)) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for ProjectTree {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ProjectTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows();
        let focused = self.focus.is_focused(window);
        let (row_height, selected_bg) = (
            theme::length(cx, "h-row"),
            theme::color(cx, if focused { "bg-tree-selected" } else { "bg-tree-selected-inactive" }),
        );
        let (fg, dimmed, chevron_color) =
            (theme::color(cx, "fg-strong"), theme::color(cx, "fg-muted"), theme::color(cx, "fg-icon"));
        let selected = self.selected.clone();
        let list = uniform_list(
            "project-tree",
            rows.len(),
            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                rows[range]
                    .iter()
                    .map(|row| {
                        let node = &row.node;
                        let chevron = if node.dir {
                            let name = if row.expanded { "chevron-down" } else { "chevron-right" };
                            let toggled = node.key.clone();
                            div()
                                .id(SharedString::from(format!("toggle:{}", node.key)))
                                .size(px(16.0))
                                .flex_none()
                                .child(theme::icon(cx, name, 16.0, chevron_color))
                                .on_click(cx.listener(move |tree, _: &ClickEvent, _, cx| {
                                    tree.toggle(&toggled, cx);
                                    cx.stop_propagation();
                                }))
                                .into_any_element()
                        } else {
                            div().size(px(16.0)).flex_none().into_any_element()
                        };
                        let color = if node.ignored || node.excluded { dimmed } else { fg };
                        let icon = match &node.icon {
                            Some(icon) => icon.as_str(),
                            None if node.dir => "folder",
                            None => theme::icons::file_icon_name(&node.label),
                        };
                        let clicked = node.key.clone();
                        let menu_key = node.key.clone();
                        div()
                            .id(SharedString::from(format!("row:{}", node.key)))
                            .debug_selector({
                                let key = node.key.clone();
                                move || format!("project-row:{key}")
                            })
                            .h(row_height)
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .pl(px(4.0 + 20.0 * row.depth as f32))
                            .pr(px(8.0))
                            .text_color(color)
                            .when(selected.as_deref() == Some(node.key.as_str()), |el| el.bg(selected_bg))
                            .child(chevron)
                            .child(theme::icon(cx, icon, 16.0, color))
                            .child(div().whitespace_nowrap().child(SharedString::from(node.label.clone())))
                            .when_some(node.detail.clone(), |el, detail| {
                                el.child(div().whitespace_nowrap().text_color(dimmed).child(SharedString::from(detail)))
                            })
                            .on_click(cx.listener(move |tree, event: &ClickEvent, window, cx| {
                                window.focus(&tree.focus, cx);
                                tree.select(&clicked, cx);
                                if event.click_count() >= 2 {
                                    tree.activate(&clicked, cx);
                                }
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |tree, event: &MouseDownEvent, window, cx| {
                                    // Handled here: the tree's own focus tracking must not take the
                                    // press, and the menu that opens takes the keyboard.
                                    window.prevent_default();
                                    cx.stop_propagation();
                                    window.focus(&tree.focus, cx);
                                    tree.select(&menu_key, cx);
                                    cx.emit(ContextMenuRequested { position: event.position });
                                }),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .text_size(theme::length(cx, "fs-ui"))
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AppDirs;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};
    use std::path::Path;

    fn settle(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..300 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the backend did not deliver within three seconds");
    }

    /// The labels of the visible rows, indented by depth.
    fn labels(tree: &Entity<ProjectTree>, cx: &mut VisualTestContext) -> Vec<String> {
        tree.read_with(cx, |tree, _| {
            tree.rows().iter().skip(1).map(|row| format!("{}{}", "  ".repeat(row.depth - 1), row.node.label)).collect()
        })
    }

    fn text(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    struct Fixture {
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        root: PathBuf,
    }

    /// A backend with the project of `files` (paths ending in `/` are folders) open in window `w1`.
    async fn project(cx: &mut TestAppContext, files: &[&str]) -> Fixture {
        cx.executor().allow_parking();
        let (home, project) = (tempfile::tempdir().expect("a home"), tempfile::tempdir().expect("a project"));
        let root = project.path().canonicalize().expect("the project folder exists");
        for file in files {
            let path = root.join(file);
            if file.ends_with('/') {
                std::fs::create_dir_all(&path).expect("a folder");
            } else {
                std::fs::create_dir_all(path.parent().expect("a parent")).expect("a folder");
                std::fs::write(&path, "").expect("a file");
            }
        }
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            cx.set_global(UiStorage::default());
            Backend::init(AppDirs::under(home.path()), cx).expect("the backend starts");
        });
        let open_root = root.clone();
        let opened = cx.update(|cx| {
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &open_root).await.map(|_| ())
            })
        });
        opened.await.expect("the task completes").expect("the project opens");
        Fixture { _home: home, _project: project, root }
    }

    fn open_tree<'a>(cx: &'a mut TestAppContext, root: &Path) -> (Entity<ProjectTree>, &'a mut VisualTestContext) {
        let tree_root = root.to_path_buf();
        let (tree, cx) = cx.add_window_view(move |_, cx| ProjectTree::new("w1", tree_root, cx));
        let at = text(root);
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.is_loaded(&at)));
        (tree, cx)
    }

    fn close(cx: &mut VisualTestContext) {
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    #[test]
    fn stored_options_fill_in_what_is_missing() {
        let options: ViewOptions = serde_json::from_str(r#"{"showExcluded":false,"sortBy":"size"}"#).expect("parses");
        assert!(!options.show_excluded && options.compact && options.folders_first);
        assert_eq!(options.sort_key(), SortKey::Name);
        assert_eq!(ViewOptions::default().tree_options(), TreeOptions::default());
        let stored = serde_json::to_value(ViewOptions::default()).expect("serializes");
        assert_eq!(stored["autoscrollFromSource"], false);
        assert_eq!(stored["sortBy"], "name");
    }

    /// The tree lists a real project through the backend, expands with the keyboard and asks
    /// for a file to be opened.
    #[gpui_kit::test]
    async fn lists_a_project_and_opens_files_from_the_keyboard(cx: &mut TestAppContext) {
        let fixture = project(cx, &["src/main.rs", "src/lib.rs", "README.md", "a10.txt", "a9.txt", "_build.txt"]).await;
        let root = fixture.root.clone();
        let (tree, cx) = open_tree(cx, &root);
        // Folders first, then files as the collation orders them: punctuation before letters,
        // a9 before a10, case ignored.
        assert_eq!(labels(&tree, cx), ["src", "_build.txt", "a9.txt", "a10.txt", "README.md"]);
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.rows()[0].node.label.clone()),
            text(root.file_name().unwrap().as_ref())
        );

        let opened: std::rc::Rc<std::cell::RefCell<Vec<PathBuf>>> = Default::default();
        let log = opened.clone();
        let _subscription = cx
            .update(|_, cx| cx.subscribe(&tree, move |_, event: &OpenFile, _| log.borrow_mut().push(event.0.clone())));
        cx.update(|window, cx| {
            let focus = tree.read(cx).focus.clone();
            window.focus(&focus, cx);
        });
        // Down to the root, down to `src`, Right expands it, Right steps onto its first file,
        // Enter opens it.
        cx.simulate_keystrokes("down down right");
        let src = text(&root.join("src"));
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.is_loaded(&src)));
        cx.simulate_keystrokes("right");
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.selected_path().map(str::to_owned)),
            Some(text(&root.join("src/lib.rs")))
        );
        cx.simulate_keystrokes("enter");
        assert_eq!(*opened.borrow(), [root.join("src/lib.rs")]);
        // Left goes to the parent folder, Left again closes it.
        cx.simulate_keystrokes("left left");
        assert_eq!(labels(&tree, cx).len(), 5);
        assert_eq!(tree.read_with(cx, |tree, _| tree.selected().map(str::to_owned)), Some(src));
        close(cx);
    }

    /// Single-folder chains show as one row: `/`-joined above a JVM source root, which ends the
    /// chain, and `.`-joined packages below it.
    #[gpui_kit::test]
    async fn compacts_chains_of_single_folders(cx: &mut TestAppContext) {
        let fixture = project(cx, &["src/main/java/com/acme/App.java", "README.md"]).await;
        let root = fixture.root.clone();
        let (tree, cx) = open_tree(cx, &root);
        assert_eq!(labels(&tree, cx), ["src", "README.md"]);
        tree.update(cx, |tree, cx| tree.toggle(&text(&root.join("src")), cx));
        let acme = text(&root.join("src/main/java/com/acme"));
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.is_loaded(&acme)));
        assert_eq!(labels(&tree, cx), ["src/main/java", "  com.acme", "README.md"]);
        // The package row is keyed by its first folder and opens its last.
        tree.update(cx, |tree, cx| tree.toggle(&text(&root.join("src/main/java/com")), cx));
        assert_eq!(labels(&tree, cx), ["src/main/java", "  com.acme", "    App.java", "README.md"]);

        // Compact Middle Packages off: every folder is a row. `src` was opened before its chain
        // was known, so the folders it later merged with are closed; the package row was opened
        // as a chain, so both of its folders are open.
        tree.update(cx, |tree, cx| tree.set_options(|options| options.compact = false, cx));
        assert_eq!(labels(&tree, cx), ["src", "  main", "README.md"]);
        tree.update(cx, |tree, cx| {
            tree.toggle(&text(&root.join("src/main")), cx);
            tree.toggle(&text(&root.join("src/main/java")), cx);
        });
        assert_eq!(
            labels(&tree, cx),
            ["src", "  main", "    java", "      com", "        acme", "          App.java", "README.md"]
        );
        let stored = cx.update(|_, cx| cx.global::<UiStorage>().get(OPTIONS_KEY).map(str::to_owned)).expect("stored");
        assert!(stored.contains(r#""compact":false"#), "{stored}");
        close(cx);
    }

    /// What `.gitignore` matches is listed dimmed, and not at all with Show Excluded Files off;
    /// ignored names (`.git`, `.DS_Store`) only show with Show Hidden Files.
    #[gpui_kit::test]
    async fn hides_excluded_and_ignored_entries_by_option(cx: &mut TestAppContext) {
        let fixture = project(cx, &["target/", "src/main.rs", ".DS_Store"]).await;
        let root = fixture.root.clone();
        std::fs::write(root.join(".gitignore"), "target/\n").expect("a file");
        let (tree, cx) = open_tree(cx, &root);
        assert_eq!(labels(&tree, cx), ["src", "target", ".gitignore"]);
        assert!(tree.read_with(cx, |tree, _| tree.rows()[2].node.ignored));
        tree.update(cx, |tree, cx| tree.set_options(|options| options.show_excluded = false, cx));
        assert_eq!(labels(&tree, cx), ["src", ".gitignore"]);
        tree.update(cx, |tree, cx| tree.set_options(|options| options.show_hidden = true, cx));
        assert_eq!(labels(&tree, cx), ["src", ".DS_Store", ".gitignore"]);

        // The ignored names are the setting's, as it stands: a name added to it goes at once.
        tree.update(cx, |tree, cx| tree.set_options(|options| options.show_hidden = false, cx));
        cx.update(|_, cx| {
            let names = serde_json::json!([".DS_Store", ".gitignore"]);
            let scope = ide_workspace::settings::SettingsScope::Global;
            crate::platform::settings::set("w1", scope, "files.ignoredNames", &names, cx).expect("stored");
        });
        settle(cx, |cx| labels(&tree, cx) == ["src"]);
        close(cx);
    }

    /// A change on disk re-reads the loaded folders it touches; a removed folder takes its rows,
    /// its listings and the selection inside it along.
    #[gpui_kit::test]
    async fn follows_changes_on_disk(cx: &mut TestAppContext) {
        let fixture = project(cx, &["src/main.rs", "docs/guide/intro.md", "README.md"]).await;
        let root = fixture.root.clone();
        let (tree, cx) = open_tree(cx, &root);
        let (docs, guide) = (text(&root.join("docs")), text(&root.join("docs/guide")));
        tree.update(cx, |tree, cx| tree.toggle(&docs, cx));
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.is_loaded(&guide)));
        assert_eq!(labels(&tree, cx), ["docs/guide", "  intro.md", "src", "README.md"]);

        // A new file in a loaded folder appears; one in a folder never read costs nothing.
        std::fs::write(root.join("NOTES.md"), "").expect("a file");
        std::fs::write(root.join("src/lib.rs"), "").expect("a file");
        let created = [text(&root.join("NOTES.md")), text(&root.join("src/lib.rs"))];
        tree.update(cx, |tree, cx| tree.apply_fs_change(&created, &[], &[], cx));
        settle(cx, |cx| labels(&tree, cx).contains(&"NOTES.md".to_owned()));
        assert_eq!(labels(&tree, cx), ["docs/guide", "  intro.md", "src", "NOTES.md", "README.md"]);
        assert!(!tree.read_with(cx, |tree, _| tree.is_loaded(&text(&root.join("src")))));

        // The same through the backend's event, for this window only.
        std::fs::write(root.join("TODO.md"), "").expect("a file");
        let event = |window: &str| BackendEvent {
            window: Some(window.to_owned()),
            event: IdeEvent::FsChanged {
                created: vec![ide_core::IpcPath(root.join("TODO.md"))],
                modified: Vec::new(),
                removed: Vec::new(),
            },
        };
        // Another window's change starts no read; the tree's own watcher may be reading meanwhile.
        tree.update(cx, |tree, cx| {
            let reads = tree.folder_reads();
            tree.backend_event(&event("w2"), cx);
            assert_eq!(tree.folder_reads(), reads, "another window's change reads nothing");
        });
        tree.update(cx, |tree, cx| tree.backend_event(&event("w1"), cx));
        settle(cx, |cx| labels(&tree, cx).contains(&"TODO.md".to_owned()));

        // Removing a loaded folder drops it, what was loaded below it and the selection in it.
        tree.update(cx, |tree, cx| tree.select(&text(&root.join("docs/guide/intro.md")), cx));
        std::fs::remove_dir_all(root.join("docs")).expect("removed");
        tree.update(cx, |tree, cx| tree.apply_fs_change(&[], &[], std::slice::from_ref(&docs), cx));
        settle(cx, |cx| !labels(&tree, cx).iter().any(|label| label.starts_with("docs")));
        assert_eq!(labels(&tree, cx), ["src", "NOTES.md", "README.md", "TODO.md"]);
        tree.read_with(cx, |tree, _| {
            assert!(!tree.is_loaded(&docs) && !tree.is_loaded(&guide));
            assert_eq!(tree.selected(), None);
        });
        close(cx);
    }

    /// A change reported while the folder is being read is read again once that read is back: the
    /// read under way may have listed the folder before the change.
    #[gpui_kit::test]
    async fn a_change_reported_while_its_folder_is_read_is_read_again_after(cx: &mut TestAppContext) {
        let fixture = project(cx, &["README.md"]).await;
        let root = fixture.root.clone();
        let (tree, cx) = open_tree(cx, &root);
        settle(cx, |cx| labels(&tree, cx).contains(&"README.md".to_owned()));
        let before = tree.read_with(cx, |tree, _| tree.folder_reads());
        let notes = text(&root.join("NOTES.md"));
        tree.update(cx, |tree, cx| {
            tree.load_dir(&text(&root), true, cx);
            std::fs::write(root.join("NOTES.md"), "").expect("a file");
            tree.apply_fs_change(std::slice::from_ref(&notes), &[], &[], cx);
            assert!(tree.read_again.contains(&text(&root)), "the folder is to be read again");
        });
        settle(cx, |cx| {
            tree.read_with(cx, |tree, _| {
                tree.inflight.is_empty() && tree.read_again.is_empty() && tree.folder_reads() >= before + 2
            })
        });
        assert!(labels(&tree, cx).contains(&"NOTES.md".to_owned()), "{:?}", labels(&tree, cx));
        close(cx);
    }

    /// An expanded folder that vanished before it was read collapses instead of staying pending.
    #[gpui_kit::test]
    async fn a_folder_that_cannot_be_found_collapses(cx: &mut TestAppContext) {
        let fixture = project(cx, &["src/main.rs", "README.md"]).await;
        let root = fixture.root.clone();
        let (tree, cx) = open_tree(cx, &root);
        let src = text(&root.join("src"));
        std::fs::remove_dir_all(root.join("src")).expect("removed");
        tree.update(cx, |tree, cx| tree.toggle(&src, cx));
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.inflight.is_empty() && !tree.expanded.contains(&src)));
        assert!(tree.read_with(cx, |tree, _| !tree.is_loaded(&src) && tree.expanded.contains(tree.root())));
        close(cx);
    }

    /// The expansion is written once it rests, and the next tree of the same project starts
    /// from it: also with the root collapsed.
    #[gpui_kit::test]
    async fn remembers_the_expansion_per_project(cx: &mut TestAppContext) {
        let fixture = project(cx, &["src/main.rs", "README.md"]).await;
        let root = fixture.root.clone();
        let key = expanded_key(&text(&root));
        {
            let (tree, cx) = open_tree(cx, &root);
            let src = text(&root.join("src"));
            tree.update(cx, |tree, cx| tree.toggle(&src, cx));
            settle(cx, |cx| tree.read_with(cx, |tree, _| tree.is_loaded(&src)));
            assert_eq!(cx.update(|_, cx| cx.global::<UiStorage>().get(&key).map(str::to_owned)), None);
            cx.executor().advance_clock(PERSIST_DELAY);
            cx.run_until_parked();
            let stored: Vec<String> = cx
                .update(|_, cx| serde_json::from_str(cx.global::<UiStorage>().get(&key).expect("stored")))
                .expect("a list");
            assert_eq!(stored, [text(&root), src]);
        }
        let (again, cx) = open_tree(cx, &root);
        let main = text(&root.join("src"));
        settle(cx, |cx| again.read_with(cx, |tree, _| tree.is_loaded(&main)));
        assert_eq!(labels(&again, cx), ["src", "  main.rs", "README.md"]);

        again.update(cx, |tree, cx| tree.toggle(&text(&root), cx));
        cx.executor().advance_clock(PERSIST_DELAY);
        cx.run_until_parked();
        let tree_root = root.clone();
        let (collapsed, cx) = cx.add_window_view(move |_, cx| ProjectTree::new("w1", tree_root, cx));
        cx.run_until_parked();
        assert_eq!(collapsed.read_with(cx, |tree, _| tree.rows().len()), 1);
        close(cx);
    }

    /// Revealing a file opens the folders above it, loads them, and selects its row; a path
    /// outside the project is refused.
    #[gpui_kit::test]
    async fn reveals_a_path_by_opening_the_folders_above_it(cx: &mut TestAppContext) {
        let fixture = project(cx, &["src/app/ui/view.rs", "src/app/model.rs", "README.md"]).await;
        let root = fixture.root.clone();
        let (tree, cx) = open_tree(cx, &root);
        let view = text(&root.join("src/app/ui/view.rs"));
        assert!(tree.update(cx, |tree, cx| tree.reveal_path(&view, cx)));
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.selected_path() == Some(view.as_str())));
        assert_eq!(labels(&tree, cx), ["src/app", "  ui", "    view.rs", "  model.rs", "README.md"]);
        assert!(!tree.update(cx, |tree, cx| tree.reveal_path("/elsewhere/file.rs", cx)));
        close(cx);
    }
}
