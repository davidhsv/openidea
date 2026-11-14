//! The File Structure popup: the symbols of one file as a tree, with a field that filters them while
//! typing. The symbol around the caret starts selected; the arrows walk and fold the tree and Enter
//! asks the window to show the selected symbol. The options under the field sort the symbols, hide
//! fields and non-public symbols and add the members inherited from supertypes.

use super::inherited::{
    HierarchyItem, INHERITED_DEPTH, InheritedMember, InheritedRequest, InheritedWalk, type_key, types_in,
};
use super::outline::{
    OutlineFilter, OutlineNode, OutlineViewFilter, apply_view_filter, filter_outline, has_fields, has_visibility,
    key_outline, parent_keys, sort_outline, symbol_path_at, visible_count, with_inherited,
};
use crate::backend::Backend;
use crate::platform::i18n::Localization;
use crate::platform::input::{self, Keymap};
use crate::platform::keymap::is_mac_host;
use crate::platform::storage::UiStorage;
use crate::platform::virtual_docs::{basename, is_virtual_uri};
use crate::theme;
use crate::ui::matching::index_runs;
use crate::ui::text_field::{TextField, TextFieldEvent};
use gpui_kit::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Keystroke, Render,
    ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};
use ide_core::error::ErrorKind;
use ide_core::{DocId, IdeResult, OutlineItem, Pos, SymbolKind};
use ide_host::Host;
use ide_lsp::types::HierarchyDirection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

/// Where the view options of the Structure views are stored.
pub const OPTIONS_KEY: &str = "openidea.structure.options.v1";
/// The action that opens the popup; its shortcut is shown beside the title.
const ACTION_ID: &str = "navigate.fileStructure";
/// Height of a row, in pixels.
const ROW_HEIGHT: f32 = 22.0;
/// How far each level of the tree is indented, in pixels.
const INDENT: f32 = 16.0;
const WIDTH: f32 = 460.0;
/// The tallest the list grows before it scrolls.
const MAX_LIST_HEIGHT: f32 = 420.0;
/// Space above and below the rows of the list.
const LIST_PADDING: f32 = 4.0;

/// The view options of the Structure views, shared by the tool window and the popup and kept
/// between sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StructureOptions {
    pub alphabetical: bool,
    /// The tool window selects the symbol at the caret.
    pub autoscroll_from_source: bool,
    /// Selecting a symbol in the tool window shows it in the editor.
    pub autoscroll_to_source: bool,
    /// Typing in the popup hides the symbols that do not match; otherwise it only highlights the
    /// ones that do.
    pub narrow_down: bool,
    pub show_fields: bool,
    pub show_non_public: bool,
    pub sort_by_visibility: bool,
    /// Members inherited from supertypes are listed under each type.
    pub show_inherited: bool,
}

impl Default for StructureOptions {
    fn default() -> Self {
        Self {
            alphabetical: false,
            autoscroll_from_source: false,
            autoscroll_to_source: true,
            narrow_down: true,
            show_fields: true,
            show_non_public: true,
            sort_by_visibility: false,
            show_inherited: false,
        }
    }
}

impl StructureOptions {
    /// The stored options; the defaults for whatever is missing or unreadable.
    pub fn stored(storage: &UiStorage) -> Self {
        storage.get(OPTIONS_KEY).and_then(|text| serde_json::from_str(text).ok()).unwrap_or_default()
    }
}

/// The file whose structure the popup shows.
#[derive(Debug, Clone)]
pub struct StructureFile {
    /// The file's path, or the URI of a virtual document.
    pub path: String,
    /// The backend's document of that file. It stays open for as long as the popup may ask about it.
    pub doc: DocId,
}

/// What the popup asks of its window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStructureEvent {
    /// Escape with an empty filter.
    Dismissed,
    /// Close the popup and show `path` with the caret at the 0-based line and UTF-16 column: the
    /// identifier of the chosen symbol. An inherited member lives in the file of its supertype.
    Reveal { path: PathBuf, line: u32, col: u32 },
}

/// One visible line of the tree.
#[derive(Debug, Clone)]
struct Row {
    key: String,
    depth: usize,
    expandable: bool,
    expanded: bool,
    kind: SymbolKind,
    name: String,
    detail: Option<String>,
    /// The supertype an inherited member comes from.
    owner: Option<String>,
    /// The parts of the name the filter matches, as byte ranges.
    runs: Vec<Range<usize>>,
    /// A filter is typed and this symbol does not match it: it is only on the way to one that does.
    dim: bool,
}

/// The inherited members of each type of a file, by [`type_key`].
type Inherited = HashMap<String, Vec<InheritedMember>>;

pub struct FileStructure {
    window_label: String,
    /// The file shown; `None` when no editor is open.
    path: Option<String>,
    /// The document the outline is read from; `None` when the outline was handed in.
    doc: Option<DocId>,
    /// Where the caret was in the file when the popup opened.
    caret: Option<Pos>,
    field: Entity<TextField>,
    options: StructureOptions,
    /// The file's symbols as the backend reports them; `None` until they arrive.
    items: Option<Vec<OutlineItem>>,
    loading: bool,
    /// The file's language server answers type hierarchy requests: Show inherited members is offered.
    inherited_supported: bool,
    inherited: Option<Inherited>,
    /// The symbols with the view options applied.
    nodes: Vec<OutlineNode>,
    /// What the typed filter leaves of `nodes`, and what it matches.
    filter: OutlineFilter,
    /// The parent of every node of `filter.nodes`.
    parents: HashMap<String, Option<String>>,
    rows: Rc<Vec<Row>>,
    /// Keys of the collapsed nodes; the popup opens fully expanded.
    collapsed: HashSet<String>,
    /// Key of the selected node.
    selected: Option<String>,
    /// The node the popup last selected by itself: the symbol at the caret, or the best match.
    preselect: Option<String>,
    /// The filter as typed.
    query: String,
    /// The questions still out with the backend; asking for the symbols again drops the old ones.
    loads: Vec<Task<()>>,
    scroll: UniformListScrollHandle,
    /// The claim on the keys the popup handles itself, ahead of the keymap.
    key_claim: u64,
    _field_events: Subscription,
}

impl EventEmitter<FileStructureEvent> for FileStructure {}

/// The supertypes of the type whose identifier is at `pos` in `doc`.
async fn supertypes_of(host: &Arc<Host>, window: &str, doc: DocId, pos: Pos) -> IdeResult<Vec<HierarchyItem>> {
    let found = ide_host::ops::lsp::lsp_type_hierarchy(host, window, doc, pos, HierarchyDirection::Incoming).await?;
    Ok(found
        .into_iter()
        .map(|item| HierarchyItem {
            name: item.name,
            path: item.path.0.to_string_lossy().into_owned(),
            selection_range: item.selection_range,
        })
        .collect())
}

/// Runs `ask` on the document of `path`, opened for as long as it runs. The backend counts the
/// openings of a document, so one that an editor shows stays open afterwards.
async fn with_doc<T, F>(host: &Arc<Host>, window: &str, path: &str, ask: impl FnOnce(DocId) -> F) -> IdeResult<T>
where
    F: Future<Output = IdeResult<T>>,
{
    let info = if is_virtual_uri(path) {
        ide_host::ops::doc::doc_open_virtual(host, window, path.to_owned()).await?
    } else {
        ide_host::ops::doc::open(host, window, PathBuf::from(path)).await?
    };
    let doc = info.id;
    let answer = ask(doc).await;
    let (inner, label) = (host.clone(), window.to_owned());
    if let Err(error) = host.blocking(move || ide_host::ops::doc::close(&inner, &label, doc)).await {
        tracing::debug!("file structure: cannot close {path}: {}", error.message);
    }
    answer
}

/// Whether a type hierarchy request for the first of `types` reaches a language server: a file
/// without a server that supports it is answered `unavailable`.
async fn probe_hierarchy(host: &Arc<Host>, window: &str, doc: DocId, types: &[OutlineItem]) -> bool {
    let Some(first) = types.first() else { return false };
    match supertypes_of(host, window, doc, first.selection_range.start).await {
        Ok(_) => true,
        Err(error) => !matches!(error.kind, ErrorKind::Unavailable | ErrorKind::Cancelled),
    }
}

/// The members `type_item`, declared in `path` (open as `doc`), inherits. A supertype whose
/// hierarchy or symbols the backend cannot give contributes nothing.
async fn inherited_of(
    host: &Arc<Host>,
    window: &str,
    path: &str,
    doc: DocId,
    type_item: &OutlineItem,
) -> Vec<InheritedMember> {
    let mut walk = InheritedWalk::new(path, type_item, INHERITED_DEPTH);
    while let Some(request) = walk.request() {
        match request {
            InheritedRequest::Supertypes { path: at, pos } => {
                let found = if at == path {
                    supertypes_of(host, window, doc, pos).await
                } else {
                    with_doc(host, window, &at, |other| supertypes_of(host, window, other, pos)).await
                };
                walk.supertypes_loaded(found.unwrap_or_default());
            }
            InheritedRequest::Outline { path: at } => {
                let found = if at == path {
                    ide_host::ops::doc::doc_outline(host, window, doc).await
                } else {
                    with_doc(host, window, &at, |other| ide_host::ops::doc::doc_outline(host, window, other)).await
                };
                walk.outline_loaded(&at, found.unwrap_or_default());
            }
        }
    }
    walk.into_members()
}

/// Whether the server of the file knows supertypes and, when it does and they are `wanted`, the
/// inherited members of each of `types`. `known` is an earlier yes for the same file: the server
/// is not probed again.
pub(crate) async fn inherited_of_types(
    host: Arc<Host>,
    window: String,
    path: String,
    doc: DocId,
    types: Vec<OutlineItem>,
    known: bool,
    wanted: bool,
) -> (bool, Option<Inherited>) {
    let supported = known || probe_hierarchy(&host, &window, doc, &types).await;
    if !supported || !wanted {
        return (supported, None);
    }
    let members =
        futures::future::join_all(types.iter().map(|type_item| inherited_of(&host, &window, &path, doc, type_item)))
            .await;
    let inherited = types.iter().map(type_key).zip(members).filter(|(_, members)| !members.is_empty()).collect();
    (true, Some(inherited))
}

/// The node with `key` among `nodes` and their descendants.
fn find<'a>(nodes: &'a [OutlineNode], key: &str) -> Option<&'a OutlineNode> {
    nodes.iter().find_map(|node| if node.key == key { Some(node) } else { find(&node.children, key) })
}

/// Appends the visible lines of `nodes`, top to bottom, to `rows`.
fn flatten(
    nodes: &[OutlineNode],
    depth: usize,
    collapsed: &HashSet<String>,
    filter: &OutlineFilter,
    filtering: bool,
    rows: &mut Vec<Row>,
) {
    for node in nodes {
        let expandable = !node.children.is_empty();
        let expanded = expandable && !collapsed.contains(&node.key);
        let matched = filter.matches.get(&node.key);
        rows.push(Row {
            key: node.key.clone(),
            depth,
            expandable,
            expanded,
            kind: node.item.kind,
            name: node.item.name.clone(),
            detail: node.item.detail.clone().filter(|detail| !detail.is_empty()),
            owner: node.inherited.as_ref().map(|from| from.owner.clone()),
            runs: matched.map(|indices| index_runs(&node.item.name, indices)).unwrap_or_default(),
            dim: filtering && matched.is_none(),
        });
        if expanded {
            flatten(&node.children, depth + 1, collapsed, filter, filtering, rows);
        }
    }
}

/// The node icon of a symbol kind. A kind without an icon of its own borrows the nearest one.
pub(super) fn symbol_icon(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::File => "symbol-file",
        SymbolKind::Module => "symbol-module",
        SymbolKind::Namespace => "symbol-namespace",
        SymbolKind::Package => "symbol-package",
        SymbolKind::Class => "symbol-class",
        SymbolKind::Interface => "symbol-interface",
        SymbolKind::Struct => "symbol-struct",
        SymbolKind::Enum => "symbol-enum",
        SymbolKind::Trait => "symbol-trait",
        SymbolKind::Function | SymbolKind::Macro | SymbolKind::Operator => "symbol-function",
        SymbolKind::Method => "symbol-method",
        SymbolKind::Constructor => "symbol-constructor",
        SymbolKind::Field => "symbol-field",
        SymbolKind::Property | SymbolKind::Event | SymbolKind::Key => "symbol-property",
        SymbolKind::Variable => "symbol-variable",
        SymbolKind::Constant | SymbolKind::EnumMember => "symbol-constant",
        SymbolKind::TypeParameter | SymbolKind::TypeAlias => "symbol-type",
        SymbolKind::Unknown => "symbol-text",
    }
}

impl FileStructure {
    /// The popup for `file`, open in the window `window_label`; its symbols are asked of the
    /// backend. `caret` is the caret's position in the file: the symbol around it starts selected.
    /// Without a file the popup says that no editor is open.
    pub fn new(window_label: &str, file: Option<StructureFile>, caret: Option<Pos>, cx: &mut Context<Self>) -> Self {
        let (path, doc) = file.map(|file| (file.path, file.doc)).unzip();
        let mut popup = Self::build(window_label, path, doc, caret, cx);
        popup.load(cx);
        popup
    }

    /// The popup for the file at `path` with `items` as its symbols. Nothing is asked of the
    /// backend, so inherited members are not offered.
    pub fn with_outline(path: &str, items: Vec<OutlineItem>, caret: Option<Pos>, cx: &mut Context<Self>) -> Self {
        let mut popup = Self::build("", Some(path.to_owned()), None, caret, cx);
        popup.outline_loaded(items, cx);
        popup
    }

    fn build(
        window_label: &str,
        path: Option<String>,
        doc: Option<DocId>,
        caret: Option<Pos>,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = cx.default_global::<Localization>().0.tx(if path.is_some() {
            "Type to filter symbols"
        } else {
            "No active editor"
        });
        let field = cx.new(|cx| TextField::new(cx).with_placeholder(placeholder));
        let events = cx.subscribe(&field, |popup: &mut FileStructure, _, event: &TextFieldEvent, cx| match event {
            // Typing shows every match: what was folded opens again.
            TextFieldEvent::Changed => popup.query_changed(true, cx),
            // Enter is the popup's own key: it is claimed before the field sees it.
            TextFieldEvent::Submitted => {}
            TextFieldEvent::Cancelled => cx.emit(FileStructureEvent::Dismissed),
        });
        let claimant = cx.weak_entity();
        let key_claim = input::claim_keys(
            field.focus_handle(cx),
            move |keystroke, _, cx| claimant.update(cx, |popup, cx| popup.claim_key(keystroke, cx)).unwrap_or(false),
            cx,
        );
        cx.on_release(|popup, cx| input::release_keys(popup.key_claim, cx)).detach();
        let options = cx.try_global::<UiStorage>().map(StructureOptions::stored).unwrap_or_default();
        Self {
            window_label: window_label.to_owned(),
            path,
            doc,
            caret,
            field,
            options,
            items: None,
            loading: false,
            inherited_supported: false,
            inherited: None,
            nodes: Vec::new(),
            filter: OutlineFilter::default(),
            parents: HashMap::new(),
            rows: Rc::default(),
            collapsed: HashSet::new(),
            selected: None,
            preselect: None,
            query: String::new(),
            loads: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            key_claim,
            _field_events: events,
        }
    }

    pub fn field(&self) -> &Entity<TextField> {
        &self.field
    }

    pub fn options(&self) -> &StructureOptions {
        &self.options
    }

    /// The symbols are being asked of the backend.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Whether Show inherited members is offered for this file.
    pub fn inherited_supported(&self) -> bool {
        self.inherited_supported
    }

    /// The visible rows as text: the symbol's name, indented two spaces per level; an inherited
    /// member names its supertype after `↑`.
    pub fn lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| {
                let owner = row.owner.as_ref().map(|owner| format!(" ↑{owner}")).unwrap_or_default();
                format!("{}{}{owner}", "  ".repeat(row.depth), row.name)
            })
            .collect()
    }

    /// The names of the visible symbols the typed filter matches, top to bottom.
    pub fn matched(&self) -> Vec<String> {
        self.rows.iter().filter(|row| !row.runs.is_empty()).map(|row| row.name.clone()).collect()
    }

    /// The name of the selected symbol.
    pub fn selected(&self) -> Option<String> {
        self.selected_node().map(|node| node.item.name.clone())
    }

    /// The key of the selected symbol: its kind and name after those of the symbols around it.
    pub fn selected_key(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected node, while the filter leaves it in the tree.
    fn selected_node(&self) -> Option<&OutlineNode> {
        find(&self.filter.nodes, self.selected.as_deref()?)
    }

    /// The inherited members the tree lists: none while they are switched off.
    fn shown_inherited(&self) -> Option<&Inherited> {
        self.inherited.as_ref().filter(|_| self.options.show_inherited && self.inherited_supported)
    }

    /// Applies the view options to the file's symbols.
    fn rebuild_nodes(&mut self) {
        let keyed = key_outline(self.items.as_deref().unwrap_or_default());
        let all = with_inherited(&keyed, self.shown_inherited());
        let view = OutlineViewFilter { fields: self.options.show_fields, non_public: self.options.show_non_public };
        self.nodes =
            sort_outline(&apply_view_filter(&all, view), self.options.alphabetical, self.options.sort_by_visibility);
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        flatten(&self.filter.nodes, 0, &self.collapsed, &self.filter, !self.query.trim().is_empty(), &mut rows);
        self.rows = Rc::new(rows);
    }

    /// Filters the symbols by what is typed and lays out the rows. The popup then selects for the
    /// user, when the tree shown is another one (`shown_changed`) or what it would select is: the
    /// best match while a filter is typed, else the symbol at the caret, else the first symbol. A
    /// node folded away is not selected, and the selection stays where it is.
    fn refresh(&mut self, shown_changed: bool, cx: &mut Context<Self>) {
        self.filter = filter_outline(&self.nodes, &self.query, self.options.narrow_down);
        self.parents = parent_keys(&self.filter.nodes);
        self.rebuild_rows();
        let preselect = if self.query.trim().is_empty() {
            self.caret.and_then(|caret| symbol_path_at(&self.nodes, caret).pop())
        } else {
            self.filter.best.clone()
        };
        if shown_changed || preselect != self.preselect {
            self.preselect.clone_from(&preselect);
            if let Some(key) = preselect.or_else(|| self.filter.nodes.first().map(|node| node.key.clone())) {
                self.reveal(&key, cx);
            }
        }
        cx.notify();
    }

    /// Selects the node with `key` and scrolls it to the middle of the list. Does nothing for a
    /// node that has no row.
    fn reveal(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(index) = self.rows.iter().position(|row| row.key == key) else { return };
        self.selected = Some(key.to_owned());
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }

    /// The text of the field is another one. Narrowing shows another tree whenever a filter was or
    /// is typed; highlighting leaves the tree as it is.
    fn query_changed(&mut self, unfold: bool, cx: &mut Context<Self>) {
        let typed = self.field.read(cx).text().to_owned();
        let narrowed = |query: &str| !query.trim().is_empty();
        let shown_changed = self.options.narrow_down && (narrowed(&self.query) || narrowed(&typed));
        self.query = typed;
        if unfold {
            self.collapsed.clear();
        }
        self.refresh(shown_changed, cx);
    }

    /// Asks the backend for the symbols of the file, then for what its types inherit.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc else { return };
        let Some(backend) = cx.try_global::<Backend>() else { return };
        let window = self.window_label.clone();
        let outline =
            backend.with_host(move |host| async move { ide_host::ops::doc::doc_outline(&host, &window, doc).await });
        self.loading = true;
        self.loads.clear();
        self.loads.push(cx.spawn(async move |popup, cx| {
            let outline = outline.await;
            // The popup is gone when it was closed meanwhile.
            let _ = popup.update(cx, |popup, cx| match outline {
                Ok(Ok(items)) => popup.outline_loaded(items, cx),
                Ok(Err(error)) => popup.outline_failed(&error.message, cx),
                Err(error) => popup.outline_failed(&error.to_string(), cx),
            });
        }));
        cx.notify();
    }

    fn outline_loaded(&mut self, items: Vec<OutlineItem>, cx: &mut Context<Self>) {
        self.loading = false;
        self.items = Some(items);
        self.rebuild_nodes();
        self.refresh(true, cx);
        self.load_inherited(cx);
    }

    /// The symbols shown stay the ones of the last answer; without one the list says it is empty.
    fn outline_failed(&mut self, message: &str, cx: &mut Context<Self>) {
        tracing::warn!("file structure: cannot read the symbols: {message}");
        self.loading = false;
        cx.notify();
    }

    /// Asks whether the file's language server knows supertypes and, with Show inherited members
    /// on, for the members each type of the file inherits.
    fn load_inherited(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(doc), Some(items)) = (self.path.clone(), self.doc, self.items.as_deref()) else {
            return;
        };
        let Some(backend) = cx.try_global::<Backend>() else { return };
        let types: Vec<OutlineItem> = types_in(items).into_iter().cloned().collect();
        let (window, known, wanted) =
            (self.window_label.clone(), self.inherited_supported, self.options.show_inherited);
        let found = backend.with_host(move |host| inherited_of_types(host, window, path, doc, types, known, wanted));
        self.loads.push(cx.spawn(async move |popup, cx| match found.await {
            Ok((supported, inherited)) => {
                // The popup is gone when it was closed meanwhile.
                let _ = popup.update(cx, |popup, cx| popup.inherited_loaded(supported, inherited, cx));
            }
            Err(error) => tracing::warn!("file structure: cannot read the inherited members: {error}"),
        }));
    }

    fn inherited_loaded(&mut self, supported: bool, inherited: Option<Inherited>, cx: &mut Context<Self>) {
        let shown = self.shown_inherited().is_some();
        self.inherited_supported = supported;
        self.inherited = inherited;
        let shown_changed = shown || self.shown_inherited().is_some();
        self.rebuild_nodes();
        self.refresh(shown_changed, cx);
    }

    /// Changes the view options, for every Structure view from now on.
    pub fn set_options(&mut self, change: impl FnOnce(&mut StructureOptions), cx: &mut Context<Self>) {
        let before = self.options.clone();
        let listed = self.shown_inherited().is_some();
        change(&mut self.options);
        if self.options == before {
            return;
        }
        if cx.has_global::<UiStorage>() {
            match serde_json::to_string(&self.options) {
                Ok(text) => cx.global_mut::<UiStorage>().set(OPTIONS_KEY, text),
                Err(error) => tracing::warn!("file structure: cannot store the options: {error}"),
            }
        }
        let now = &self.options;
        let view_changed = (before.alphabetical, before.sort_by_visibility, before.show_fields, before.show_non_public)
            != (now.alphabetical, now.sort_by_visibility, now.show_fields, now.show_non_public);
        let narrowing_changed = before.narrow_down != now.narrow_down && !self.query.trim().is_empty();
        let inherited_changed = before.show_inherited != now.show_inherited;
        let shown_changed = view_changed || narrowing_changed || listed != self.shown_inherited().is_some();
        self.rebuild_nodes();
        self.refresh(shown_changed, cx);
        if inherited_changed {
            self.load(cx);
        }
    }

    /// The File Structure action, invoked while the popup is open: lists or hides the inherited
    /// members, when the file's language server knows supertypes.
    pub fn toggle_show_inherited(&mut self, cx: &mut Context<Self>) {
        if self.inherited_supported {
            self.set_options(|options| options.show_inherited = !options.show_inherited, cx);
        }
    }

    fn set_expanded(&mut self, key: &str, open: bool, cx: &mut Context<Self>) {
        if open {
            self.collapsed.remove(key);
        } else {
            self.collapsed.insert(key.to_owned());
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Folds or unfolds the node with `key`. A selection inside a node that folds moves to it.
    pub fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        let open = self.collapsed.contains(key);
        if !open {
            let above = |key: &String| self.parents.get(key).cloned().flatten();
            let mut ancestor = self.selected.as_ref().and_then(above);
            while let Some(at) = ancestor {
                if at == key {
                    self.selected = Some(key.to_owned());
                    break;
                }
                ancestor = self.parents.get(&at).cloned().flatten();
            }
        }
        self.set_expanded(key, open, cx);
    }

    /// Selects the node with `key` where it is, without scrolling.
    pub fn select(&mut self, key: &str, cx: &mut Context<Self>) {
        self.selected = Some(key.to_owned());
        cx.notify();
    }

    /// Moves the selection one row up or down; it stops at the ends. While a filter only highlights,
    /// it goes from match to match instead, round the ends, and unfolds what hides the next match.
    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let highlighting = !self.query.trim().is_empty() && !self.options.narrow_down && !self.filter.order.is_empty();
        if highlighting {
            let order = &self.filter.order;
            let last = order.len() - 1;
            let at = self.selected.as_ref().and_then(|selected| order.iter().position(|key| key == selected));
            let next = match at {
                None if forward => 0,
                None => last,
                Some(at) if forward => (at + 1) % order.len(),
                Some(0) => last,
                Some(at) => at - 1,
            };
            let key = order[next].clone();
            let mut ancestor = self.parents.get(&key).cloned().flatten();
            while let Some(at) = ancestor {
                self.collapsed.remove(&at);
                ancestor = self.parents.get(&at).cloned().flatten();
            }
            self.rebuild_rows();
            self.reveal(&key, cx);
            cx.notify();
            return;
        }
        let at = self.selected.as_ref().and_then(|selected| self.rows.iter().position(|row| &row.key == selected));
        let next = match at {
            None => 0,
            Some(at) if forward => at + 1,
            Some(0) => return,
            Some(at) => at - 1,
        };
        let Some(row) = self.rows.get(next) else { return };
        self.selected = Some(row.key.clone());
        self.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Asks the window to show the symbol with `key`.
    pub fn choose(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(path) = self.path.as_deref() else { return };
        let Some(node) = find(&self.filter.nodes, key) else { return };
        let at = node.item.selection_range.start;
        let path = PathBuf::from(node.inherited.as_ref().map_or(path, |from| from.path.as_str()));
        cx.emit(FileStructureEvent::Reveal { path, line: at.line, col: at.col });
    }

    /// The keys the popup handles itself while the keyboard is in its field, in every modifier
    /// combination; the field and the keymap do not see them. Returns whether `keystroke` is one
    /// of them. Left and Right belong to the tree while a symbol they can act on is selected, else
    /// they move the caret in the field. Escape empties the field first; with an empty field it
    /// is the field's, which dismisses the popup.
    fn claim_key(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        match keystroke.key.as_str() {
            "enter" => {
                if let Some(key) = self.selected_node().map(|node| node.key.clone()) {
                    self.choose(&key, cx);
                }
            }
            "escape" => {
                if self.query.is_empty() {
                    return false;
                }
                self.field.update(cx, |field, cx| field.set_text("", cx));
                self.query_changed(false, cx);
            }
            "up" => self.step(false, cx),
            "down" => self.step(true, cx),
            "right" => {
                let parent = self.selected_node().filter(|node| !node.children.is_empty());
                let Some((key, first)) = parent.map(|node| (node.key.clone(), node.children[0].key.clone())) else {
                    return false;
                };
                if self.collapsed.contains(&key) {
                    self.set_expanded(&key, true, cx);
                } else {
                    self.reveal(&first, cx);
                }
            }
            "left" => {
                let Some((key, parent)) =
                    self.selected_node().map(|node| (node.key.clone(), !node.children.is_empty()))
                else {
                    return false;
                };
                if parent && !self.collapsed.contains(&key) {
                    self.set_expanded(&key, false, cx);
                } else if let Some(above) = self.parents.get(&key).cloned().flatten() {
                    self.reveal(&above, cx);
                }
            }
            _ => return false,
        }
        true
    }
}

impl Focusable for FileStructure {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.field.focus_handle(cx)
    }
}

impl Render for FileStructure {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let i18n = &cx.default_global::<Localization>().0;
        let title = match &self.path {
            Some(path) => basename(path).into_owned(),
            None => i18n.tx("File Structure"),
        };
        let empty = i18n.tx(match (&self.path, self.loading && self.items.is_none()) {
            (None, _) => "Open a file first",
            (Some(_), true) => "Loading…",
            (Some(_), false) => "No symbols",
        });
        let narrow_label = i18n.tx("Narrow down on typing");
        let fields_label = i18n.tx("Show fields");
        let non_public_label = i18n.tx("Show non-public");
        let inherited_label = i18n.tx("Show inherited members");
        let hint = cx.try_global::<Keymap>().and_then(|keymap| keymap.0.shortcut_label(ACTION_ID, is_mac_host()));

        let (fg, muted, subtle, faint) = (
            theme::color(cx, "fg-strong"),
            theme::color(cx, "fg-muted"),
            theme::color(cx, "fg-subtle"),
            theme::color(cx, "fg-faint"),
        );
        let (chevron_color, divider) = (theme::color(cx, "fg-icon"), theme::color(cx, "border-popup"));
        // The keyboard is in the field, not in the tree: the selection is drawn as an inactive one.
        let (selected_bg, hover_bg) =
            (theme::color(cx, "bg-tree-selected-inactive"), theme::color(cx, "bg-tree-hover"));
        let (match_bg, match_fg) = (theme::color(cx, "bg-search-match"), theme::color(cx, "fg-search-match"));
        let (pressed_bg, button_hover) = (theme::color(cx, "bg-selected"), theme::color(cx, "bg-hover"));
        let (box_on, box_off, box_border, check) = (
            theme::color(cx, "bg-button-primary"),
            theme::color(cx, "bg-checkbox"),
            theme::color(cx, "border-checkbox"),
            theme::color(cx, "fg-button-primary"),
        );

        let options = self.options.clone();
        let items = self.items.as_deref().unwrap_or_default();
        let (offers_fields, offers_non_public, offers_inherited) =
            (has_fields(items) || !options.show_fields, has_visibility(items), self.inherited_supported);
        let height = (visible_count(&self.filter.nodes, &self.collapsed).max(1) as f32 * ROW_HEIGHT
            + 2.0 * LIST_PADDING)
            .min(MAX_LIST_HEIGHT);

        let rows = Rc::clone(&self.rows);
        let selected = self.selected.clone();
        let body =
            if rows.is_empty() {
                div().px(px(12.0)).py(px(4.0)).text_color(faint).child(SharedString::from(empty)).into_any_element()
            } else {
                uniform_list(
                    "file-structure",
                    rows.len(),
                    cx.processor(move |_, range: Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let row = &rows[index];
                                let chevron = if row.expandable {
                                    let name = if row.expanded { "chevron-down" } else { "chevron-right" };
                                    let toggled = row.key.clone();
                                    div()
                                        .id("toggle")
                                        .debug_selector(|| format!("file-structure-toggle-{index}"))
                                        .size(px(16.0))
                                        .flex_none()
                                        .child(theme::icon(cx, name, 16.0, chevron_color))
                                        .on_click(cx.listener(move |popup, _: &ClickEvent, _, cx| {
                                            popup.toggle(&toggled, cx);
                                            cx.stop_propagation();
                                        }))
                                        .into_any_element()
                                } else {
                                    div().size(px(16.0)).flex_none().into_any_element()
                                };
                                // The name with each matched part on the match colours.
                                let mut parts = Vec::new();
                                let mut at = 0;
                                for run in row.runs.iter().filter(|run| run.end <= row.name.len()) {
                                    if run.start < at {
                                        continue;
                                    }
                                    if run.start > at {
                                        parts.push(div().child(SharedString::from(row.name[at..run.start].to_owned())));
                                    }
                                    parts.push(
                                        div()
                                            .bg(match_bg)
                                            .text_color(match_fg)
                                            .rounded(px(3.0))
                                            .child(SharedString::from(row.name[run.clone()].to_owned())),
                                    );
                                    at = run.end;
                                }
                                if at < row.name.len() || parts.is_empty() {
                                    parts.push(div().child(SharedString::from(row.name[at..].to_owned())));
                                }
                                // An inherited member is greyed; so is a symbol that is only on the way to a match.
                                let name_color = if row.owner.is_some() {
                                    muted
                                } else if row.dim {
                                    subtle
                                } else {
                                    fg
                                };
                                let is_selected = selected.as_deref() == Some(row.key.as_str());
                                let clicked = row.key.clone();
                                div()
                                    .id(index)
                                    .debug_selector(|| format!("file-structure-row-{index}"))
                                    .h(px(ROW_HEIGHT))
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .pl(px(6.0 + INDENT * row.depth as f32))
                                    .pr(px(8.0))
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_color(fg)
                                    .when(is_selected, |el| el.bg(selected_bg))
                                    .when(!is_selected, |el| el.hover(|el| el.bg(hover_bg)))
                                    .child(chevron)
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.0))
                                            .min_w_0()
                                            .child(theme::icon(cx, symbol_icon(row.kind), 16.0, fg))
                                            .child(div().flex().flex_none().text_color(name_color).children(parts))
                                            .children(row.detail.clone().map(|detail| {
                                                div().text_color(subtle).child(SharedString::from(detail))
                                            }))
                                            .children(row.owner.as_ref().map(|owner| {
                                                div().text_color(subtle).child(SharedString::from(format!("↑{owner}")))
                                            })),
                                    )
                                    .on_click(cx.listener(move |popup, event: &ClickEvent, _, cx| {
                                        popup.select(&clicked, cx);
                                        if event.click_count() >= 2 {
                                            popup.choose(&clicked, cx);
                                        }
                                    }))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .size_full()
                .into_any_element()
            };

        let checkbox = |id: &'static str, label: String, on: bool, change: fn(&mut StructureOptions)| {
            let mark = div().size(px(16.0)).flex_none().flex().items_center().justify_center().rounded(px(3.0));
            let mark = if on {
                mark.bg(box_on).child(theme::icon(cx, "check", 12.0, check))
            } else {
                mark.bg(box_off).border_1().border_color(box_border)
            };
            div()
                .id(id)
                .debug_selector(move || format!("file-structure-{id}"))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_color(fg)
                .child(mark)
                .child(SharedString::from(label))
                .on_click(cx.listener(move |popup, _: &ClickEvent, _, cx| popup.set_options(change, cx)))
        };
        let option_row = div()
            .flex_none()
            .h(px(26.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(12.0))
            .border_b_1()
            .border_color(divider)
            .text_size(theme::length(cx, "fs-small"))
            .child(checkbox("narrow", narrow_label, options.narrow_down, |options| {
                options.narrow_down = !options.narrow_down;
            }))
            .children(offers_fields.then(|| {
                checkbox("fields", fields_label, options.show_fields, |options| {
                    options.show_fields = !options.show_fields;
                })
            }))
            .children(offers_non_public.then(|| {
                checkbox("non-public", non_public_label, options.show_non_public, |options| {
                    options.show_non_public = !options.show_non_public;
                })
            }))
            .children(offers_inherited.then(|| {
                checkbox("inherited", inherited_label, options.show_inherited, |options| {
                    options.show_inherited = !options.show_inherited;
                })
            }));
        let sort = div()
            .id("sort")
            .debug_selector(|| "file-structure-sort".to_owned())
            .flex_none()
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .when(options.alphabetical, |el| el.bg(pressed_bg))
            .hover(|el| el.bg(button_hover))
            .child(theme::icon(cx, "sort", 16.0, if options.alphabetical { fg } else { muted }))
            .on_click(cx.listener(|popup, _: &ClickEvent, _, cx| {
                popup.set_options(|options| options.alphabetical = !options.alphabetical, cx);
            }));

        div()
            .w(px(WIDTH))
            .flex()
            .flex_col()
            .bg(theme::color(cx, "bg-popup"))
            .border_1()
            .border_color(theme::color(cx, "border"))
            .rounded(px(10.0))
            .overflow_hidden()
            .text_size(theme::length(cx, "fs-ui"))
            .child(
                div()
                    .flex_none()
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .text_color(fg)
                    .child(SharedString::from(title))
                    .children(hint.map(|hint| {
                        div().text_color(faint).text_size(theme::length(cx, "fs-small")).child(SharedString::from(hint))
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(12.0))
                    .pr(px(8.0))
                    .border_b_1()
                    .border_color(divider)
                    .child(div().flex_1().min_w_0().child(self.field.clone()))
                    .child(sort),
            )
            .child(option_row)
            .child(div().flex_none().h(px(height)).py(px(LIST_PADDING)).child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AppDirs;
    use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, TestAppContext, VisualTestContext};
    use ide_core::Visibility;
    use std::cell::RefCell;
    use std::time::Duration;
    mod proofs8_file_structure;

    const PATH: &str = "/project/src/orders.rs";

    type Events = Rc<RefCell<Vec<FileStructureEvent>>>;

    fn item(name: &str, kind: SymbolKind, from: u32, to: u32, children: Vec<OutlineItem>) -> OutlineItem {
        let len = name.len() as u32;
        OutlineItem {
            name: name.to_owned(),
            detail: None,
            kind,
            range: ide_core::Range::new(Pos::new(from, 0), Pos::new(to, 1)),
            selection_range: ide_core::Range::new(Pos::new(from, 4), Pos::new(from, 4 + len)),
            children,
            visibility: None,
        }
    }

    /// A class with three methods, two of them of one name, and a function after it.
    fn outline() -> Vec<OutlineItem> {
        let members = vec![
            item("list", SymbolKind::Method, 2, 6, vec![]),
            item("create", SymbolKind::Method, 8, 12, vec![]),
            item("list", SymbolKind::Method, 14, 16, vec![]),
        ];
        vec![
            item("OrderController", SymbolKind::Class, 0, 20, members),
            item("helper", SymbolKind::Function, 22, 25, vec![]),
        ]
    }

    fn vis(item: OutlineItem, visibility: Visibility) -> OutlineItem {
        OutlineItem { visibility: Some(visibility), ..item }
    }

    /// A class with fields and with members of every visibility.
    fn repo() -> OutlineItem {
        let members = vec![
            vis(item("cache", SymbolKind::Field, 1, 1, vec![]), Visibility::Private),
            vis(item("find", SymbolKind::Method, 2, 5, vec![]), Visibility::Public),
            vis(item("load", SymbolKind::Method, 6, 9, vec![]), Visibility::Private),
            vis(item("check", SymbolKind::Method, 10, 12, vec![]), Visibility::Protected),
            item("size", SymbolKind::Property, 13, 13, vec![]),
            item("all", SymbolKind::Method, 14, 16, vec![]),
        ];
        item("Repo", SymbolKind::Class, 0, 30, members)
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            input::init(cx);
            cx.set_global(UiStorage::default());
        });
    }

    /// Gives the popup the keyboard and collects what it asks of its window.
    fn watch(popup: &Entity<FileStructure>, cx: &mut VisualTestContext) -> Events {
        let events: Events = Rc::default();
        let log = events.clone();
        cx.update(|window, cx| {
            cx.subscribe(popup, move |_, event: &FileStructureEvent, _| log.borrow_mut().push(event.clone())).detach();
            let focus = popup.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        events
    }

    /// The popup on `items`, with the caret at `caret` of the file.
    fn open(
        cx: &mut TestAppContext,
        items: Vec<OutlineItem>,
        caret: Option<Pos>,
    ) -> (Entity<FileStructure>, Events, &mut VisualTestContext) {
        init(cx);
        let (popup, cx) = cx.add_window_view(move |_, cx| FileStructure::with_outline(PATH, items, caret, cx));
        let events = watch(&popup, cx);
        (popup, events, cx)
    }

    fn lines(popup: &Entity<FileStructure>, cx: &mut VisualTestContext) -> Vec<String> {
        popup.read_with(cx, |popup, _| popup.lines())
    }

    fn selected(popup: &Entity<FileStructure>, cx: &mut VisualTestContext) -> Option<String> {
        popup.read_with(cx, |popup, _| popup.selected())
    }

    fn selected_key(popup: &Entity<FileStructure>, cx: &mut VisualTestContext) -> Option<String> {
        popup.read_with(cx, |popup, _| popup.selected_key().map(str::to_owned))
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str, count: usize) {
        cx.run_until_parked();
        let position = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn")).center();
        cx.simulate_event(MouseDownEvent {
            position,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: count,
        });
    }

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

    #[test]
    fn stored_options_fill_in_what_is_missing() {
        let options: StructureOptions =
            serde_json::from_str(r#"{"narrowDown":false,"alphabetical":true}"#).expect("parses");
        assert!(!options.narrow_down && options.alphabetical);
        assert!(options.show_fields && options.show_non_public && options.autoscroll_to_source);
        assert!(!options.show_inherited && !options.sort_by_visibility && !options.autoscroll_from_source);
        let stored = serde_json::to_value(StructureOptions::default()).expect("serializes");
        assert_eq!(stored["autoscrollToSource"], true);
        assert_eq!(stored["showNonPublic"], true);
        assert_eq!(stored["sortByVisibility"], false);
    }

    /// The symbols show as a tree, fully unfolded, with the symbol around the caret selected.
    #[gpui_kit::test]
    async fn shows_the_outline_as_a_tree_with_the_symbol_at_the_caret_selected(cx: &mut TestAppContext) {
        let (popup, events, cx) = open(cx, outline(), Some(Pos::new(9, 3)));
        assert_eq!(lines(&popup, cx), ["OrderController", "  list", "  create", "  list", "helper"]);
        assert_eq!(selected(&popup, cx).as_deref(), Some("create"));
        assert!(cx.update(|window, cx| popup.focus_handle(cx).is_focused(window)), "the filter has the keyboard");
        assert!(events.borrow().is_empty());
        // The options that would do nothing for this file are not offered.
        cx.run_until_parked();
        assert!(cx.debug_bounds("file-structure-narrow").is_some());
        assert!(cx.debug_bounds("file-structure-fields").is_none());
        assert!(cx.debug_bounds("file-structure-non-public").is_none());
        assert!(cx.debug_bounds("file-structure-inherited").is_none());
    }

    /// A caret outside every symbol, or no caret at all, leaves the first symbol selected.
    #[gpui_kit::test]
    async fn selects_the_first_symbol_when_the_caret_is_in_none(cx: &mut TestAppContext) {
        let (popup, _, cx) = open(cx, outline(), Some(Pos::new(21, 0)));
        assert_eq!(selected(&popup, cx).as_deref(), Some("OrderController"));
        let (popup, cx) = cx.add_window_view(|_, cx| FileStructure::with_outline(PATH, Vec::new(), None, cx));
        assert!(lines(&popup, cx).is_empty());
        assert_eq!(selected(&popup, cx), None);
    }

    /// Up and Down walk the rows and stop at the ends; Left folds a node or goes to its parent,
    /// Right unfolds it or goes to its first child.
    #[gpui_kit::test]
    async fn the_arrows_walk_fold_and_unfold_the_tree(cx: &mut TestAppContext) {
        let (popup, _, cx) = open(cx, outline(), Some(Pos::new(9, 3)));
        cx.simulate_keystrokes("up");
        assert_eq!(selected(&popup, cx).as_deref(), Some("list"));
        cx.simulate_keystrokes("up up up");
        assert_eq!(selected(&popup, cx).as_deref(), Some("OrderController"));

        cx.simulate_keystrokes("left");
        assert_eq!(lines(&popup, cx), ["OrderController", "helper"]);
        assert_eq!(selected(&popup, cx).as_deref(), Some("OrderController"));
        cx.simulate_keystrokes("down down down");
        assert_eq!(selected(&popup, cx).as_deref(), Some("helper"));
        cx.simulate_keystrokes("up right");
        assert_eq!(lines(&popup, cx).len(), 5);
        assert_eq!(selected(&popup, cx).as_deref(), Some("OrderController"));
        cx.simulate_keystrokes("right");
        assert_eq!(selected(&popup, cx).as_deref(), Some("list"));
        cx.simulate_keystrokes("down left");
        assert_eq!(selected(&popup, cx).as_deref(), Some("OrderController"), "Left on a member goes to its type");
        assert_eq!(lines(&popup, cx).len(), 5);
    }

    /// Typing keeps the matches and what they are in, selects the best match and unfolds what was
    /// folded. Escape empties the filter first and dismisses the popup then.
    #[gpui_kit::test]
    async fn typing_narrows_the_tree_and_escape_clears_the_filter_before_it_dismisses(cx: &mut TestAppContext) {
        let (popup, events, cx) = open(cx, outline(), None);
        cx.simulate_keystrokes("left");
        assert_eq!(lines(&popup, cx), ["OrderController", "helper"]);
        cx.simulate_input("li");
        assert_eq!(lines(&popup, cx), ["OrderController", "  list", "  list"]);
        assert_eq!(popup.read_with(cx, |popup, _| popup.matched()), ["list", "list"]);
        assert_eq!(selected_key(&popup, cx).as_deref(), Some("/class:OrderController/method:list"));
        cx.simulate_input("x");
        assert!(lines(&popup, cx).is_empty(), "nothing is named lix");
        cx.simulate_keystrokes("backspace backspace backspace");
        cx.simulate_input("creat");
        assert_eq!(lines(&popup, cx), ["OrderController", "  create"]);
        assert_eq!(selected(&popup, cx).as_deref(), Some("create"));

        cx.simulate_keystrokes("escape");
        assert_eq!(popup.read_with(cx, |popup, cx| popup.field().read(cx).text().to_owned()), "");
        assert_eq!(lines(&popup, cx).len(), 5);
        assert_eq!(selected(&popup, cx).as_deref(), Some("OrderController"));
        assert!(events.borrow().is_empty(), "the first Escape only empties the filter");
        cx.simulate_keystrokes("escape");
        assert_eq!(*events.borrow(), [FileStructureEvent::Dismissed]);
    }

    /// Without Narrow down on typing every symbol stays; the matches are highlighted and Up and
    /// Down go from one to the next, round the ends, unfolding what hides them.
    #[gpui_kit::test]
    async fn without_narrowing_typing_highlights_and_the_arrows_go_from_match_to_match(cx: &mut TestAppContext) {
        let (popup, _, cx) = open(cx, outline(), None);
        popup.update(cx, |popup, cx| popup.set_options(|options| options.narrow_down = false, cx));
        cx.simulate_input("list");
        assert_eq!(lines(&popup, cx).len(), 5);
        assert_eq!(popup.read_with(cx, |popup, _| popup.matched()), ["list", "list"]);
        let (first, second) = ("/class:OrderController/method:list", "/class:OrderController/method:list#1");
        assert_eq!(selected_key(&popup, cx).as_deref(), Some(first));
        cx.simulate_keystrokes("down");
        assert_eq!(selected_key(&popup, cx).as_deref(), Some(second));
        cx.simulate_keystrokes("down");
        assert_eq!(selected_key(&popup, cx).as_deref(), Some(first));
        cx.simulate_keystrokes("up");
        assert_eq!(selected_key(&popup, cx).as_deref(), Some(second));

        cx.simulate_keystrokes("left left");
        assert_eq!(lines(&popup, cx), ["OrderController", "helper"]);
        cx.simulate_keystrokes("down");
        assert_eq!(lines(&popup, cx).len(), 5, "the type unfolds for the match in it");
        assert_eq!(selected_key(&popup, cx).as_deref(), Some(first));

        // Narrowing again hides what does not match.
        click(cx, "file-structure-narrow", 1);
        assert_eq!(lines(&popup, cx), ["OrderController", "  list", "  list"]);
    }

    /// The sort button and the visibility options change the tree and are stored for the next
    /// time.
    #[gpui_kit::test]
    async fn sorts_and_hides_symbols_by_the_view_options(cx: &mut TestAppContext) {
        let (popup, _, cx) = open(cx, vec![repo()], None);
        assert_eq!(lines(&popup, cx), ["Repo", "  cache", "  find", "  load", "  check", "  size", "  all"]);
        click(cx, "file-structure-sort", 1);
        assert_eq!(lines(&popup, cx), ["Repo", "  all", "  cache", "  check", "  find", "  load", "  size"]);
        let stored: serde_json::Value = cx
            .update(|_, cx| serde_json::from_str(cx.global::<UiStorage>().get(OPTIONS_KEY).expect("stored")))
            .expect("the stored options are JSON");
        assert_eq!(stored["alphabetical"], true);
        assert_eq!(stored["narrowDown"], true);
        assert_eq!(stored["autoscrollToSource"], true);

        click(cx, "file-structure-fields", 1);
        assert_eq!(lines(&popup, cx), ["Repo", "  all", "  check", "  find", "  load"]);
        click(cx, "file-structure-non-public", 1);
        assert_eq!(lines(&popup, cx), ["Repo", "  all", "  find"]);
        assert!(popup.read_with(cx, |popup, _| !popup.options().show_fields && !popup.options().show_non_public));
        // An option that is off stays offered, so that it can be switched on again.
        click(cx, "file-structure-fields", 1);
        click(cx, "file-structure-non-public", 1);
        click(cx, "file-structure-sort", 1);
        assert_eq!(lines(&popup, cx), ["Repo", "  cache", "  find", "  load", "  check", "  size", "  all"]);

        // The next popup opens with the stored options.
        popup.update(cx, |popup, cx| popup.set_options(|options| options.sort_by_visibility = true, cx));
        let (again, cx) = cx.add_window_view(|_, cx| FileStructure::with_outline(PATH, vec![repo()], None, cx));
        assert_eq!(lines(&again, cx), ["Repo", "  find", "  size", "  all", "  check", "  cache", "  load"]);
    }

    /// Enter asks the window to show the selected symbol at its identifier.
    #[gpui_kit::test]
    async fn enter_reveals_the_selected_symbol(cx: &mut TestAppContext) {
        let (_popup, events, cx) = open(cx, outline(), Some(Pos::new(9, 3)));
        cx.simulate_keystrokes("enter");
        assert_eq!(*events.borrow(), [FileStructureEvent::Reveal { path: PathBuf::from(PATH), line: 8, col: 4 }]);
        cx.simulate_keystrokes("down down enter");
        assert_eq!(
            events.borrow().last(),
            Some(&FileStructureEvent::Reveal { path: PathBuf::from(PATH), line: 22, col: 4 })
        );
    }

    /// Escape with an empty filter dismisses the popup at once.
    #[gpui_kit::test]
    async fn escape_dismisses_the_popup(cx: &mut TestAppContext) {
        let (_popup, events, cx) = open(cx, outline(), None);
        cx.simulate_keystrokes("escape");
        assert_eq!(*events.borrow(), [FileStructureEvent::Dismissed]);
    }

    /// A click selects a row, a double click reveals its symbol, and the arrow of a type folds it,
    /// taking the selection out of what it hides.
    #[gpui_kit::test]
    async fn the_mouse_selects_folds_and_reveals(cx: &mut TestAppContext) {
        let (popup, events, cx) = open(cx, outline(), None);
        click(cx, "file-structure-row-2", 1);
        assert_eq!(selected(&popup, cx).as_deref(), Some("create"));
        assert!(events.borrow().is_empty());
        click(cx, "file-structure-toggle-0", 1);
        assert_eq!(lines(&popup, cx), ["OrderController", "helper"]);
        assert_eq!(selected(&popup, cx).as_deref(), Some("OrderController"));
        click(cx, "file-structure-toggle-0", 1);
        assert_eq!(lines(&popup, cx).len(), 5);
        click(cx, "file-structure-row-4", 2);
        assert_eq!(selected(&popup, cx).as_deref(), Some("helper"));
        assert_eq!(*events.borrow(), [FileStructureEvent::Reveal { path: PathBuf::from(PATH), line: 22, col: 4 }]);
    }

    /// Inherited members are listed under their type, name their supertype and are revealed in its
    /// file. The action invoked again switches them, once the server is known to give supertypes.
    #[gpui_kit::test]
    async fn lists_inherited_members_and_reveals_them_in_the_file_of_their_supertype(cx: &mut TestAppContext) {
        let base = "/project/src/base.rs";
        let (popup, events, cx) = open(cx, outline(), Some(Pos::new(15, 0)));
        popup.update(cx, |popup, cx| popup.toggle_show_inherited(cx));
        assert!(!popup.read_with(cx, |popup, _| popup.options().show_inherited), "not offered: nothing changes");

        let member = InheritedMember {
            item: item("save", SymbolKind::Method, 3, 5, vec![]),
            path: base.to_owned(),
            owner: "Base".to_owned(),
        };
        let inherited = HashMap::from([(type_key(&outline()[0]), vec![member])]);
        popup.update(cx, |popup, cx| popup.inherited_loaded(true, Some(inherited), cx));
        assert_eq!(lines(&popup, cx).len(), 5, "inherited members are off by default");
        cx.run_until_parked();
        assert!(cx.debug_bounds("file-structure-inherited").is_some());

        popup.update(cx, |popup, cx| popup.toggle_show_inherited(cx));
        assert!(popup.read_with(cx, |popup, _| popup.options().show_inherited));
        assert_eq!(lines(&popup, cx), ["OrderController", "  list", "  create", "  list", "  save ↑Base", "helper"]);
        assert_eq!(selected_key(&popup, cx).as_deref(), Some("/class:OrderController/method:list#1"));
        cx.simulate_keystrokes("down enter");
        assert_eq!(*events.borrow(), [FileStructureEvent::Reveal { path: PathBuf::from(base), line: 3, col: 4 }]);

        popup.update(cx, |popup, cx| popup.toggle_show_inherited(cx));
        assert_eq!(lines(&popup, cx).len(), 5);
    }

    /// The popup reads the symbols of a file through the real backend: a Makefile's variables and
    /// targets come from its syntax tree, without a language server.
    #[gpui_kit::test]
    async fn loads_the_symbols_of_a_file_from_the_backend(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init(cx);
        let (home, project) = (tempfile::tempdir().expect("a home"), tempfile::tempdir().expect("a project"));
        let root = project.path().canonicalize().expect("the project folder exists");
        let file = root.join("Makefile");
        std::fs::write(&file, "CC := gcc\nall: build\nbuild: main.o\n\t$(CC) -o app main.o\n")
            .expect("the file is written");
        cx.update(|cx| Backend::init(AppDirs::under(home.path()), cx)).expect("the backend starts");
        let (open_root, open_file) = (root.clone(), file.clone());
        let opened = cx.update(|cx| {
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &open_root).await?;
                ide_host::ops::doc::open(&host, "w1", open_file).await
            })
        });
        let info = opened.await.expect("the task completes").expect("the file opens");

        let source = StructureFile { path: file.to_string_lossy().into_owned(), doc: info.id };
        let (popup, cx) =
            cx.add_window_view(move |_, cx| FileStructure::new("w1", Some(source), Some(Pos::new(3, 2)), cx));
        let events = watch(&popup, cx);
        settle(cx, |cx| popup.read_with(cx, |popup, _| !popup.is_loading() && !popup.lines().is_empty()));
        assert_eq!(lines(&popup, cx), ["CC", "all", "build"]);
        assert_eq!(selected(&popup, cx).as_deref(), Some("build"), "the caret is in the recipe of build");
        assert!(!popup.read_with(cx, |popup, _| popup.inherited_supported()));
        cx.simulate_keystrokes("enter");
        assert_eq!(*events.borrow(), [FileStructureEvent::Reveal { path: file.clone(), line: 2, col: 0 }]);

        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }
}
