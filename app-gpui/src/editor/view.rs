//! The editor view: one document of the backend, painted natively.
//!
//! The text is the backend's own rope. [`Editor`] holds a clone of it at a known version (a
//! rope clone shares its chunks, so the text exists once in the process) and every edit goes
//! through `Documents::edit` with that version, exactly as the webview's `doc_edit` does, only
//! without a copy of the document on the UI side. Edits the backend makes itself (formatting,
//! refactorings, reloads from disk) arrive as `DocPatched` events and move the selection.
//!
//! The selection is one or more ranges, one of them the main one: typing, deleting and moving
//! act on every range, and an edit of several ranges is one edit of the document and one undo
//! step.
//!
//! Only the visible lines are shaped and painted, so the cost of a frame does not depend on the
//! length of the document.

use super::buffer::{self, Assoc};
use super::command_actions::{CommandMemory, recall};
use super::history::{self, EditKind, History, invert};
use super::language_facts::EditorFacts;
use super::marks::{Mark, MarkLayer, map_marks};
use crate::backend::Backend;
use crate::editor_commands::clipboard_history::ClipboardHistory;
use crate::editor_commands::smart_keys::{self, PairScope};
use crate::editor_commands::tree::{SyntaxTree, TreeNode};
use crate::editor_commands::{
    self as commands, Edit, EditState, RangeEdit, Selection, SelectionExt, caret, change, change_by_range, editing,
    vertical_motion,
};
use crate::platform::input::{self, EDITOR_CONTEXT};
use crate::platform::keymap::normalize_binding;
use crate::theme::{self, installed_fonts};
use gpui_kit::MouseMoveEvent;
use gpui_kit::{
    AnyView, App, Bounds, ClipboardItem, Context, DispatchPhase, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, Font, FontFallbacks, FontFeatures, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, KeyDownEvent, LayoutId, Modifiers, MouseButton,
    MouseDownEvent, Pixels, Point, Render, ScrollEvent, ShapedLine, SharedString, Style, TextAlign, TextRun,
    UTF16Selection, WeakEntity, Window, div, fill, point, prelude::*, px, quad, relative, size,
};
use ide_core::error::ErrorKind;
use ide_core::{ChangeSet, DocId, IdeEvent, IdeResult, LanguageId, TextChange};
use ide_host::Host;
use ide_language::SyntaxHighlightKind;
use ide_text::LineEnding;
use ide_workspace::{DocumentInfo, EditorConfigProps};
use ropey::Rope;
use std::collections::HashMap;
use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

/// The colour token of a highlight kind, as the webview's highlight style assigns them.
pub fn syntax_token(kind: SyntaxHighlightKind) -> &'static str {
    use SyntaxHighlightKind as K;
    match kind {
        K::Keyword => "syn-keyword",
        K::String | K::StringSpecial => "syn-string",
        K::Escape => "syn-escape",
        K::Regexp => "syn-regexp",
        K::Atom | K::Constant | K::Label => "syn-constant",
        K::Number => "syn-number",
        K::Comment => "syn-comment",
        K::DocComment => "syn-doc-comment",
        K::Function => "syn-function",
        K::FunctionCall => "syn-function-call",
        K::Builtin => "syn-builtin",
        K::Type | K::Namespace => "syn-type",
        K::Variable => "syn-variable",
        K::SelfVariable => "syn-self",
        K::Property => "syn-property",
        K::Attribute | K::Macro => "syn-decorator",
        K::Tag => "syn-tag",
        K::Operator | K::Punctuation => "syn-punct",
        K::Parameter => "syn-param",
    }
}

/// How long the text must be still before it is highlighted again.
const HIGHLIGHT_DELAY: std::time::Duration = std::time::Duration::from_millis(40);

/// The action whose mouse binding adds or removes a caret where the editor is clicked.
const ADD_CARET_ACTION: &str = "editor.addOrRemoveCaret";

/// The width of the fold chevrons' column, the gutter's last before the text, right of the line
/// numbers, the breakpoints and the plugins' icons.
pub const FOLD_GUTTER: f32 = 16.0;

/// The colours Rainbow brackets cycle through by depth.
const BRACKET_COLORS: [&str; 3] = ["syn-bracket-1", "syn-bracket-2", "syn-bracket-3"];

/// Whether Distraction Free Mode is on: the editors draw no gutter and centre their text.
fn distraction_free(cx: &App) -> bool {
    crate::workbench::distraction_free_active(cx)
}

/// A highlighted range: UTF-16 offsets and its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub from: u32,
    pub to: u32,
    pub kind: SyntaxHighlightKind,
}

/// The active editor's state as the status bar shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorStatus {
    /// Zero-based line and UTF-16 column of the caret.
    pub line: u32,
    pub col: u32,
    /// Length of the selection in UTF-16 code units, and the line breaks it spans.
    pub selected_chars: u32,
    pub selected_line_breaks: u32,
    pub line_ending: LineEnding,
    pub encoding: String,
    pub read_only: bool,
}

impl EditorStatus {
    /// `12:5`, with the selection's size when there is one: `12:5 (40 chars, 2 line breaks)`.
    pub fn caret_text(&self) -> String {
        let mut text = format!("{}:{}", self.line + 1, self.col + 1);
        if self.selected_chars > 0 {
            text.push_str(&format!(" ({} chars", self.selected_chars));
            match self.selected_line_breaks {
                0 => {}
                1 => text.push_str(", 1 line break"),
                n => text.push_str(&format!(", {n} line breaks")),
            }
            text.push(')');
        }
        text
    }

    pub fn line_ending_text(&self) -> &'static str {
        match self.line_ending {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
            LineEnding::Cr => "CR",
        }
    }
}

/// What the last frame drew, for hit-testing and the platform's text input.
struct Layout {
    bounds: Bounds<Pixels>,
    /// Width of the gutter, the fold chevrons' column included; none in Distraction Free Mode.
    gutter: Pixels,
    /// Width of the fold chevrons' column, the gutter's last.
    fold_column: Pixels,
    /// Left edge of the text: right of the gutter, or of the margin that centres it.
    text_left: Pixels,
    line_height: Pixels,
    /// The first painted row (a row is a line no fold hides).
    first_line: u32,
    lines: Vec<(String, ShapedLine)>,
    /// The document line of each painted row.
    line_of: Vec<u32>,
    /// Where the last frame drew the line tints, breakpoint dots and problem underlines; recorded for
    /// tests only, so a frame allocates nothing for it.
    #[cfg(test)]
    painted_marks: Vec<(super::marks::MarkKind, Bounds<Pixels>)>,
    /// The colour runs (UTF-8 lengths) each painted row was shaped with; for tests only.
    #[cfg(test)]
    painted_colors: Vec<Vec<(usize, Hsla)>>,
}

/// The editors that show each document. The backend reports the edits it makes itself, not the
/// ones an editor sends it: an editor tells the others of its document about those.
#[derive(Default)]
struct DocumentEditors(std::collections::HashMap<DocId, Vec<WeakEntity<Editor>>>);

impl gpui_kit::Global for DocumentEditors {}

/// What every editor copied or cut, newest first: the latest entry tells whether the clipboard
/// holds a line-wise copy.
#[derive(Default)]
pub struct EditorClipboard(pub ClipboardHistory);

impl gpui_kit::Global for EditorClipboard {}

/// Asked first when Escape is pressed in the editor; answers whether it used the key (it closed
/// something the editor's tab shows).
pub type EscapeHandler = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

/// Asked first when the editor is pressed, with the offset under the pointer: answers whether it
/// took the press (a navigation), which then neither moves the caret nor selects. It runs while the
/// editor is being updated, so it must not read or update the editor itself.
pub type PointerHandler = Rc<dyn Fn(&MouseDownEvent, Option<u32>, &mut Window, &mut App) -> bool>;

/// What made the text change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOrigin {
    /// Typing: the text typed at every caret (a line break for Enter).
    Typed(String),
    /// Backspace, Delete or deleting by word or to the line start.
    Deleted,
    /// An accepted completion item.
    Completion,
    /// Undo or redo.
    History,
    /// The backend changed the document itself (formatting, a refactoring, a reload, another
    /// editor of the document).
    External,
    /// Any other command of this editor (paste, line commands, an edit applied by its owner).
    Other,
}

/// What the editor tells its owner.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorEvent {
    /// The text changed by `changes` (in the coordinates of the text before them); the carets
    /// moved with them.
    Edited { origin: EditOrigin, changes: Vec<TextChange> },
    /// The carets or selections moved without an edit.
    SelectionMoved,
    /// The pointer rests over the text at `offset` with `modifiers` held, or left the text (`None`).
    Hovered { offset: Option<u32>, modifiers: Modifiers },
    /// The gutter left of the text was pressed on the 0-based `line`, with Shift or Alt held.
    GutterPressed { line: u32, shift: bool, alt: bool },
}

impl gpui_kit::EventEmitter<EditorEvent> for Editor {}

/// Where a caret-adding mouse gesture started: the selection before it, and the press offset.
struct AddingCaret {
    ranges: Vec<Selection>,
    main: usize,
    anchor: u32,
}

pub struct Editor {
    focus: FocusHandle,
    host: Arc<Host>,
    /// The backend's label for the window whose project owns the document.
    window_label: String,
    doc: DocId,
    path: PathBuf,
    language: LanguageId,
    read_only: bool,
    large: bool,
    line_ending: LineEnding,
    encoding: String,
    editorconfig: Option<EditorConfigProps>,
    rope: Rope,
    version: u32,
    /// The selection ranges in UTF-16 offsets, `(anchor, head)` each, sorted and merged as
    /// [`commands::normalize`] does; never empty. `main` indexes the main one.
    selections: Vec<Selection>,
    main: usize,
    /// Text being composed by an input method, at the main caret.
    marked: Option<Range<u32>>,
    scroll_top: Pixels,
    pub(super) highlights: Vec<HighlightSpan>,
    /// The version `highlights` were computed for.
    pub(super) highlights_version: u32,
    /// Ranges drawn behind or over the text, by who set them.
    marks: HashMap<MarkLayer, Vec<Mark>>,
    /// The language's facts and the settings the commands read.
    facts: EditorFacts,
    /// What the commands remember between two invocations.
    pub(super) memory: CommandMemory,
    /// The bracket and quote pairs the editor inserted, that Tab steps out of.
    pub(super) pair_scopes: Vec<PairScope>,
    /// Overwrite mode: typed characters replace the ones after the caret.
    pub(super) overwrite: bool,
    escape: Option<EscapeHandler>,
    pointer: Option<PointerHandler>,
    /// Drawn over the text by the editor's owner: popups anchored at offsets of the text.
    overlay: Option<AnyView>,
    /// What the next edit of [`Editor::edit_with`] is, when it is typing.
    next_origin: Option<EditOrigin>,
    /// The offset last reported under the pointer, so that a move within it reports nothing.
    hovered: (Option<u32>, Modifiers),
    layout: Option<Layout>,
    listener: u64,
    history: History,
    /// Where a drag selection began, while the mouse button is down.
    drag_anchor: Option<u32>,
    /// The pointer is over the gutter: the open folds' chevrons show.
    gutter_hovered: bool,
    /// The brackets of the text with their depths, for Rainbow brackets.
    pub(super) brackets: std::cell::RefCell<super::extras::BracketCache>,
    /// A caret-adding press, while the mouse button is down.
    adding: Option<AddingCaret>,
    /// Where a ⌥-drag column selection began, while the mouse button is down.
    column_drag: Option<commands::column_mode::BlockPoint>,
    /// The document has changes that are not on disk.
    dirty: bool,
    /// Counts edits, so that a highlight request an edit has overtaken is dropped.
    generation: u64,
    created: std::time::Instant,
    /// Folding, Extend / Shrink Selection and the font zoom ([`super::core_actions`]).
    pub(super) core: super::core_actions::CoreState,
}

impl Editor {
    /// An editor for a document the backend has open for `window_label`.
    pub fn new(host: Arc<Host>, window_label: &str, info: &DocumentInfo, cx: &mut Context<Self>) -> IdeResult<Self> {
        let (version, rope) = host.project(window_label)?.documents.rope_snapshot(info.id)?;
        Ok(Self::with_text(host, window_label, info, version, rope, cx))
    }

    /// An editor showing `rope`, the document's text at `version`.
    pub fn with_text(
        host: Arc<Host>,
        window_label: &str,
        info: &DocumentInfo,
        version: u32,
        rope: Rope,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak: WeakEntity<Self> = cx.weak_entity();
        let doc = info.id;
        cx.default_global::<DocumentEditors>().0.entry(doc).or_default().push(weak.clone());
        let listener = cx.global_mut::<Backend>().listen(move |event, cx| match &event.event {
            IdeEvent::DocPatched { doc: patched, base_version, changes, workspace_edit, .. } if *patched == doc => {
                let _ = weak.update(cx, |editor, cx| editor.remote_patch(*base_version, changes, *workspace_edit, cx));
            }
            IdeEvent::DocDirty { doc: changed, dirty } if *changed == doc => {
                let _ = weak.update(cx, |editor, cx| {
                    editor.dirty = *dirty;
                    cx.notify();
                });
            }
            IdeEvent::SettingsChanged { .. } => {
                let _ = weak.update(cx, |editor, cx| {
                    editor.read_facts();
                    // The font may have changed.
                    cx.notify();
                });
            }
            _ => {}
        });
        cx.on_release(|editor, cx| {
            let shown = &mut cx.default_global::<DocumentEditors>().0;
            if let Some(editors) = shown.get_mut(&editor.doc) {
                editors.retain(|other| other.upgrade().is_some());
                if editors.is_empty() {
                    shown.remove(&editor.doc);
                }
            }
            cx.global_mut::<Backend>().unlisten(editor.listener);
            // Every editor holds one reference to its document; the backend drops the document
            // with its last reference. A project that is already closed has closed it.
            if let Ok(documents) = editor.documents() {
                documents.close(editor.doc);
            }
        })
        .detach();
        let facts = EditorFacts::read(&host, window_label, &info.language, info.editorconfig.as_ref());
        let mut editor = Self {
            focus: cx.focus_handle(),
            host,
            window_label: window_label.to_owned(),
            doc,
            path: PathBuf::from(info.path.clone()),
            language: info.language.clone(),
            read_only: info.read_only,
            large: info.large,
            line_ending: info.line_ending,
            encoding: info.encoding.clone(),
            editorconfig: info.editorconfig.clone(),
            rope,
            version,
            selections: vec![caret(0)],
            main: 0,
            marked: None,
            scroll_top: px(0.0),
            highlights: Vec::new(),
            highlights_version: version,
            marks: HashMap::new(),
            facts,
            memory: CommandMemory::default(),
            pair_scopes: Vec::new(),
            overwrite: false,
            escape: None,
            pointer: None,
            overlay: None,
            next_origin: None,
            hovered: (None, Modifiers::default()),
            layout: None,
            listener,
            history: History::default(),
            drag_anchor: None,
            gutter_hovered: false,
            brackets: Default::default(),
            adding: None,
            column_drag: None,
            dirty: info.dirty,
            generation: 0,
            created: std::time::Instant::now(),
            core: Default::default(),
        };
        editor.refresh_highlights(cx);
        editor
    }

    /// Reads the language's facts and the settings the commands follow again.
    fn read_facts(&mut self) {
        self.facts = EditorFacts::read(&self.host, &self.window_label, &self.language, self.editorconfig.as_ref());
    }

    pub fn doc(&self) -> DocId {
        self.doc
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn language(&self) -> &LanguageId {
        &self.language
    }

    /// The language's facts and the settings the editing commands read.
    pub fn facts(&self) -> &EditorFacts {
        &self.facts
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn line_count(&self) -> u32 {
        buffer::line_count(&self.rope)
    }

    /// The main selection range.
    fn main_range(&self) -> Selection {
        self.selections.get(self.main).copied().unwrap_or_default()
    }

    /// The main selection as `from..to` UTF-16 offsets.
    pub fn selection(&self) -> Range<u32> {
        let main = self.main_range();
        main.start()..main.end()
    }

    /// Every selection range, `(anchor, head)` in UTF-16 offsets, in document order.
    pub fn selections(&self) -> &[(u32, u32)] {
        &self.selections
    }

    /// The index of the main range in [`Editor::selections`].
    pub fn main_index(&self) -> usize {
        self.main
    }

    /// The main selection as zero-based lines and UTF-16 columns: what the backend and plugins take.
    pub fn selection_range(&self) -> ide_core::Range {
        let selection = self.selection();
        let position = |offset| {
            let (line, col) = buffer::line_col(&self.rope, offset);
            ide_core::Pos { line, col }
        };
        ide_core::Range { start: position(selection.start), end: position(selection.end) }
    }

    /// The UTF-16 offset of the zero-based `line` and UTF-16 column `col`, clamped to the text.
    pub fn offset_of(&self, line: u32, col: u32) -> u32 {
        let line = line.min(self.line_count().saturating_sub(1));
        buffer::offset_at(&self.rope, line, col).min(buffer::line_range(&self.rope, line).end)
    }

    /// The main selection's fixed end, as a UTF-16 offset: the caret is its moving end.
    pub fn anchor(&self) -> u32 {
        self.main_range().anchor()
    }

    /// The first line the view shows, as scrolled.
    pub fn top_line(&self, cx: &App) -> u32 {
        self.core.folds.line_of_row((self.scroll_top / self.metrics(cx).1).floor() as u32)
    }

    /// Scrolls so that `line` is the first one shown (the last line, if the document is shorter).
    pub fn scroll_to_line(&mut self, line: u32, cx: &mut Context<Self>) {
        let line = line.min(self.line_count().saturating_sub(1));
        self.scroll_top = self.metrics(cx).1 * self.core.folds.row_of_line(line) as f32;
        cx.notify();
    }

    /// The main caret's UTF-16 offset.
    pub fn caret(&self) -> u32 {
        self.main_range().head()
    }

    /// What the status bar shows about this editor.
    pub fn status(&self) -> EditorStatus {
        let selection = self.selection();
        let (line, col) = self.caret_position();
        let line_of = |offset| buffer::line_col(&self.rope, offset).0;
        EditorStatus {
            line,
            col,
            selected_chars: selection.end - selection.start,
            selected_line_breaks: line_of(selection.end) - line_of(selection.start),
            line_ending: self.line_ending,
            encoding: self.encoding.clone(),
            read_only: self.read_only,
        }
    }

    /// The text of the zero-based `line`, without its line break; empty past the last line.
    pub fn line_text(&self, line: u32) -> String {
        if line >= self.line_count() { String::new() } else { buffer::line_text(&self.rope, line) }
    }

    /// The main caret's zero-based line and UTF-16 column.
    pub fn caret_position(&self) -> (u32, u32) {
        buffer::line_col(&self.rope, self.caret())
    }

    pub fn highlights(&self) -> &[HighlightSpan] {
        &self.highlights
    }

    pub fn scroll_top(&self) -> Pixels {
        self.scroll_top
    }

    /// The document lines the last frame painted, from the first to the last (the lines a fold
    /// hides between them included).
    pub fn painted_lines(&self) -> Option<Range<u32>> {
        let layout = self.layout.as_ref()?;
        Some(layout.line_of.first().copied().unwrap_or(0)..layout.line_of.last().map_or(0, |last| last + 1))
    }

    /// How many rows the last frame painted.
    pub fn painted_rows(&self) -> u32 {
        self.layout.as_ref().map_or(0, |layout| layout.lines.len() as u32)
    }

    /// Where the last frame painted the editor, gutter included.
    pub fn painted_bounds(&self) -> Option<Bounds<Pixels>> {
        self.layout.as_ref().map(|layout| layout.bounds)
    }

    /// Where the last frame drew marks of `kind`: a tinted row, a breakpoint dot, a problem's underline.
    #[cfg(test)]
    pub fn painted_marks(&self, kind: super::marks::MarkKind) -> Vec<Bounds<Pixels>> {
        let painted = self.layout.as_ref().map(|layout| layout.painted_marks.as_slice()).unwrap_or_default();
        painted.iter().filter(|(painted, _)| *painted == kind).map(|(_, bounds)| *bounds).collect()
    }

    /// The colour the last frame drew the character at `col` (UTF-16) of `line` in, if it painted
    /// the line.
    #[cfg(test)]
    pub fn painted_color_at(&self, line: u32, col: u32) -> Option<Hsla> {
        let layout = self.layout.as_ref()?;
        let row = layout.line_of.iter().position(|painted| *painted == line)?;
        let (text, _) = layout.lines.get(row)?;
        let byte = buffer::byte_of_col(text, col);
        let mut at = 0;
        layout.painted_colors.get(row)?.iter().find_map(|&(len, color)| {
            at += len;
            (byte < at).then_some(color)
        })
    }

    /// The marks `layer` set, in document order.
    pub fn marks(&self, layer: MarkLayer) -> &[Mark] {
        self.marks.get(&layer).map_or(&[], Vec::as_slice)
    }

    /// Replaces the marks of `layer`; they follow the edits of the document until replaced again.
    pub fn set_marks(&mut self, layer: MarkLayer, mut marks: Vec<Mark>, cx: &mut Context<Self>) {
        let len = buffer::len_utf16(&self.rope);
        marks.retain(|mark| mark.range.start < mark.range.end && mark.range.end <= len);
        marks.sort_by_key(|mark| (mark.range.start, mark.range.end));
        if self.marks(layer) == marks.as_slice() {
            return;
        }
        if marks.is_empty() {
            self.marks.remove(&layer);
        } else {
            self.marks.insert(layer, marks);
        }
        cx.notify();
    }

    /// What Escape asks first: `handler` answers whether it used the key.
    pub fn set_escape_handler(&mut self, handler: Option<EscapeHandler>) {
        self.escape = handler;
    }

    /// What a press asks first; see [`PointerHandler`].
    pub fn set_pointer_handler(&mut self, handler: Option<PointerHandler>) {
        self.pointer = handler;
    }

    /// Draws `overlay` over the text. Its elements place themselves in window coordinates
    /// ([`Editor::position_of`], `deferred(anchored().position(..))`) and take no room; the
    /// keyboard stays with the editor.
    pub fn set_overlay(&mut self, overlay: Option<AnyView>, cx: &mut Context<Self>) {
        self.overlay = overlay;
        cx.notify();
    }

    /// What the owner draws over the text.
    pub fn overlay(&self) -> Option<&AnyView> {
        self.overlay.as_ref()
    }

    /// The height of a line of text.
    pub fn line_height(&self, cx: &App) -> Pixels {
        self.metrics(cx).1
    }

    /// Whether the editor accepts edits.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The backend's label of the window whose project owns the document.
    pub fn window_label(&self) -> &str {
        &self.window_label
    }

    /// The text at the current version.
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// The font size, line height and gutter width the editor paints with: the theme's, the font
    /// and the line height scaled by the editor's font zoom. Distraction Free Mode has no gutter.
    fn metrics(&self, cx: &App) -> (Pixels, Pixels, Pixels) {
        let font = &self.facts.font;
        let size = if font.size > 0.0 { px(font.size) } else { theme::length(cx, "fs-code") };
        let line = if font.line_height > 0.0 { px(font.line_height) } else { theme::length(cx, "lh-code") };
        let zoomed = (size + px(self.core.font_zoom as f32)).max(px(1.0));
        let gutter =
            if distraction_free(cx) { px(0.0) } else { theme::length(cx, "w-gutter") + self.fold_column_width(cx) };
        (zoomed, line * (zoomed / size), gutter)
    }

    /// The width of the fold chevrons' column: none while Code Folding › Show code folding outline
    /// is off, or in Distraction Free Mode.
    pub fn fold_column_width(&self, cx: &App) -> Pixels {
        if self.facts.fold_outline && !distraction_free(cx) { px(FOLD_GUTTER) } else { px(0.0) }
    }

    /// The font size the editor paints with.
    pub fn font_size(&self, cx: &App) -> Pixels {
        self.metrics(cx).0
    }

    /// Makes the editor accept edits or refuse them, as its file now is.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if self.read_only != read_only {
            self.read_only = read_only;
            cx.notify();
        }
    }

    /// Scroll to Center: the main caret's row goes to the middle of the view.
    pub fn scroll_caret_to_center(&mut self, cx: &mut Context<Self>) {
        let line_height = self.metrics(cx).1;
        let viewport = self.layout.as_ref().map_or(px(0.0), |layout| layout.bounds.size.height);
        let row = self.core.folds.row_of_line(self.caret_position().0);
        let content = line_height * self.core.folds.row_count(self.line_count()) as f32;
        let top = line_height * row as f32 + line_height / 2.0 - viewport / 2.0;
        self.scroll_top = top.clamp(px(0.0), (content - viewport).max(px(0.0)));
        cx.notify();
    }

    /// The font the text is drawn with: the first family that is installed of the one Editor ›
    /// Font names, its fallback font and the theme's `--font-mono` list, with the others after it
    /// for characters it lacks; the main weight; ligatures only when they are turned on.
    pub fn code_font(&self, cx: &App) -> Font {
        let settings = &self.facts.font;
        let theme =
            cx.try_global::<theme::ActiveTheme>().map(|t| t.theme.font_families("font-mono")).unwrap_or_default();
        let theme_default = theme.first().cloned();
        let mut candidates: Vec<String> = [settings.family.clone(), settings.fallback.clone()]
            .into_iter()
            .filter(|name| !name.is_empty())
            .chain(theme)
            .collect();
        candidates.dedup();
        // Until the installed families are known, the theme's default family draws the text.
        let chosen = match installed_fonts::installed(cx) {
            Some(installed) => candidates.iter().position(|name| installed.contains(name)),
            None => candidates.iter().position(|name| theme_default.as_ref() == Some(name)),
        };
        let family = chosen.map_or_else(|| installed_fonts::last_resort().to_owned(), |at| candidates.remove(at));
        Font {
            family: SharedString::from(family),
            features: if settings.ligatures { FontFeatures::default() } else { FontFeatures::disable_ligatures() },
            fallbacks: (!candidates.is_empty()).then(|| FontFallbacks::from_fonts(candidates)),
            weight: FontWeight(f32::from(settings.weight.clamp(100, 900))),
            style: FontStyle::Normal,
        }
    }

    /// The font, size and line height the text is drawn with.
    pub fn text_style(&self, cx: &App) -> (Font, Pixels, Pixels) {
        let (size, line_height, _) = self.metrics(cx);
        (self.code_font(cx), size, line_height)
    }

    /// The document's line break as its text holds it.
    fn line_break(&self) -> &'static str {
        let end = buffer::line_range(&self.rope, 0).end;
        if buffer::slice(&self.rope, end, end + 2) == "\r\n" { "\r\n" } else { "\n" }
    }

    /// The text, the selection and the indentation settings, as the editing commands read them.
    pub fn edit_state(&self) -> EditState {
        EditState {
            rope: self.rope.clone(),
            selections: self.selections.clone(),
            main: self.main,
            tab_size: self.facts.tab_size,
            indent_unit: self.facts.indent_unit.clone(),
            line_break: self.line_break().to_owned(),
        }
    }

    /// The document's comments and string literals as a tree of their nodes alone, from the
    /// highlights of the current text. Empty (not parsed) while the highlights are of an older
    /// version, or for a document without highlights.
    pub fn literal_tree(&self) -> SyntaxTree {
        use super::find_model::literal_ranges;
        use crate::platform::search::LiteralKind;
        use SyntaxHighlightKind as K;
        if self.highlights_version != self.version || self.highlights.is_empty() {
            return SyntaxTree::default();
        }
        let literals = literal_ranges(self.highlights.iter().filter_map(|span| {
            let kind = match span.kind {
                K::Comment | K::DocComment => LiteralKind::Comment,
                K::String | K::StringSpecial | K::Escape => LiteralKind::String,
                _ => return None,
            };
            Some((span.from, span.to, kind))
        }));
        let mut nodes = vec![TreeNode::new("Document", 0, buffer::len_utf16(&self.rope))];
        nodes.extend(literals.iter().map(|literal| {
            let name = if literal.kind == LiteralKind::Comment { "Comment" } else { "String" };
            TreeNode::new(name, literal.from, literal.to)
        }));
        SyntaxTree::new(nodes)
    }

    /// The texts of the other open editors.
    pub(super) fn other_texts(&self, cx: &Context<Self>) -> Vec<String> {
        let me = cx.entity_id();
        let Some(editors) = cx.try_global::<DocumentEditors>() else { return Vec::new() };
        let others = editors.0.values().flatten().filter(|other| other.entity_id() != me);
        others.filter_map(WeakEntity::upgrade).map(|other| other.read(cx).text()).collect()
    }

    /// Where the editor's state is now: what remembered command state is valid for.
    pub(super) fn stamp(&self) -> super::command_actions::Stamp {
        (self.version, self.selections.clone(), self.main)
    }

    /// Sets a single selection; offsets are clamped to the document.
    pub fn select(&mut self, anchor: u32, head: u32, cx: &mut Context<Self>) {
        self.set_selections(vec![(anchor, head)], 0, cx);
    }

    /// Sets the selection ranges (clamped to the document, sorted and merged where they touch)
    /// and which of them is the main one.
    pub fn set_selections(&mut self, ranges: Vec<(u32, u32)>, main: usize, cx: &mut Context<Self>) {
        let before = (self.selections.clone(), self.main);
        self.put_selections(ranges, main);
        // A caret inside a fold opens it: no caret is hidden.
        let heads: Vec<u32> = self.selections.iter().map(|range| range.head()).collect();
        let state = self.edit_state();
        self.core.folds.reveal(&state, &heads);
        self.history.seal();
        self.reveal_caret(cx);
        if before != (self.selections.clone(), self.main) {
            cx.emit(EditorEvent::SelectionMoved);
        }
        cx.notify();
    }

    fn put_selections(&mut self, ranges: Vec<(u32, u32)>, main: usize) {
        let len = buffer::len_utf16(&self.rope);
        let ranges = ranges.into_iter().map(|(anchor, head)| (anchor.min(len), head.min(len))).collect();
        (self.selections, self.main) = commands::normalize(ranges, main);
    }

    /// Moves every range: its head goes where `target` says, the anchor stays with `extend`.
    fn move_carets(&mut self, extend: bool, cx: &mut Context<Self>, target: impl Fn(&Self, Selection) -> u32) {
        let ranges = self
            .selections
            .iter()
            .map(|range| {
                let head = target(self, *range);
                if extend { (range.anchor(), head) } else { caret(head) }
            })
            .collect();
        self.set_selections(ranges, self.main, cx);
    }

    /// Scrolls so that the main caret's line is inside the viewport.
    fn reveal_caret(&mut self, cx: &App) {
        let Some(layout) = &self.layout else { return };
        let (_, line_height, _) = self.metrics(cx);
        let line_top = line_height * self.core.folds.row_of_line(self.caret_position().0) as f32;
        let height = layout.bounds.size.height;
        if line_top < self.scroll_top {
            self.scroll_top = line_top;
        } else if line_top + line_height > self.scroll_top + height {
            self.scroll_top = line_top + line_height - height;
        }
    }

    /// Scrolls by `delta` pixels (positive moves the text up), clamped to the document.
    pub fn scroll_by(&mut self, delta: Pixels, cx: &mut Context<Self>) {
        let (_, line_height, _) = self.metrics(cx);
        let viewport = self.layout.as_ref().map_or(px(0.0), |l| l.bounds.size.height);
        let content = line_height * self.core.folds.row_count(self.line_count()) as f32;
        let max = (content - viewport).max(px(0.0));
        let next = (self.scroll_top + delta).clamp(px(0.0), max);
        if next != self.scroll_top {
            self.scroll_top = next;
            cx.notify();
        }
    }

    fn documents(&self) -> IdeResult<Arc<ide_workspace::Documents>> {
        Ok(self.host.project(&self.window_label)?.documents.clone())
    }

    fn snapshot(&mut self) -> IdeResult<()> {
        (self.version, self.rope) = self.documents()?.rope_snapshot(self.doc)?;
        Ok(())
    }

    /// Takes the backend's current text; the selection follows through `changes` when they are
    /// known, else through the difference between the old and the new text, unless the edit
    /// says where it goes (`selection`). Marks and the pairs the editor inserted follow too.
    /// `record` makes the change an undo entry of that kind. Returns the changes that were
    /// followed.
    fn follow(
        &mut self,
        changes: Option<&[TextChange]>,
        record: Option<EditKind>,
        selection: Option<(Vec<Selection>, usize)>,
        origin: EditOrigin,
        cx: &mut Context<Self>,
    ) -> Vec<TextChange> {
        let old = self.rope.clone();
        if let Err(error) = self.snapshot() {
            tracing::warn!("cannot read document {}: {}", self.doc, error.message);
            return Vec::new();
        }
        let changes = match changes {
            Some(changes) => changes.to_vec(),
            None => difference(&old, &self.rope),
        };
        let before_state = EditState { rope: old.clone(), ..self.edit_state() };
        let before = history::Selection { ranges: self.selections.clone(), main: self.main };
        let (ranges, main) = selection.unwrap_or_else(|| {
            let mapped = self.selections.iter().map(|&(anchor, head)| {
                (buffer::map_offset(anchor, &changes, Assoc::After), buffer::map_offset(head, &changes, Assoc::After))
            });
            (mapped.collect(), self.main)
        });
        self.put_selections(ranges, main);
        let after = self.edit_state();
        self.core.folds.map(&after, &changes);
        let heads: Vec<u32> = self.selections.iter().map(|range| range.head()).collect();
        self.core.folds.reveal(&after, &heads);
        self.marked = None;
        for marks in self.marks.values_mut() {
            *marks = map_marks(marks, &changes);
        }
        self.marks.retain(|_, marks| !marks.is_empty());
        // The colours move with the text until the new highlights arrive; a range whose text is gone goes.
        if !changes.is_empty() {
            self.highlights = self
                .highlights
                .iter()
                .filter_map(|span| {
                    let from = buffer::map_offset(span.from, &changes, Assoc::After);
                    let to = buffer::map_offset(span.to, &changes, Assoc::Before);
                    (to > from).then_some(HighlightSpan { from, to, kind: span.kind })
                })
                .collect();
        }
        let followed = Edit { changes: changes.clone(), selections: self.selections.clone(), main: self.main };
        let typed = record == Some(EditKind::Insert);
        self.pair_scopes = smart_keys::track_pair_scopes(&self.pair_scopes, &before_state, &followed, typed);
        if let Some(kind) = record.filter(|_| !changes.is_empty()) {
            let now = self.created.elapsed().as_secs_f64() * 1000.0;
            let after = history::Selection { ranges: self.selections.clone(), main: self.main };
            self.history.record(changes.clone(), invert(&old, &changes), before, after, kind, now);
        }
        self.generation += 1;
        self.refresh_highlights(cx);
        if !changes.is_empty() {
            cx.emit(EditorEvent::Edited { origin, changes: changes.clone() });
        }
        cx.notify();
        changes
    }

    /// Tells the other editors of the document about `changes`, which this one made to the text
    /// at `base_version`: they follow them as they follow the backend's own edits.
    fn tell_other_editors(&self, base_version: u32, changes: &[TextChange], cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let others: Vec<Entity<Editor>> = cx
            .default_global::<DocumentEditors>()
            .0
            .get(&self.doc)
            .into_iter()
            .flatten()
            .filter(|other| **other != this)
            .filter_map(WeakEntity::upgrade)
            .collect();
        for other in others {
            other.update(cx, |other, cx| other.remote_patch(base_version, changes, false, cx));
        }
    }

    /// The backend edited the document itself. Like any edit it can be undone here, unless it
    /// is part of a workspace edit, which is undone as a whole elsewhere: then the changes this
    /// editor could undo no longer apply to the text, and its history starts over.
    fn remote_patch(
        &mut self,
        base_version: u32,
        changes: &[TextChange],
        workspace_edit: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self.documents().and_then(|d| d.version(self.doc)).unwrap_or(self.version);
        if current == self.version {
            return; // already reflected (the edit was ours, or a snapshot overtook the event)
        }
        if workspace_edit {
            self.history.clear();
        }
        let record = (!workspace_edit).then_some(EditKind::Other);
        self.follow((base_version == self.version).then_some(changes), record, None, EditOrigin::External, cx);
    }

    /// Sends `edit` (changes in the coordinates of the current text) to the backend as one
    /// edit; the selection becomes the edit's. Answers the backend's verdict.
    fn send(&mut self, edit: Edit, kind: EditKind, origin: EditOrigin, cx: &mut Context<Self>) -> IdeResult<()> {
        let changes = ChangeSet { changes: edit.changes.clone() };
        self.documents().and_then(|d| d.edit(self.doc, self.version, changes))?;
        // Known here and now: the backend's own report arrives a moment later, and closing in
        // between must still see the document as unsaved.
        self.dirty = true;
        let base = self.version;
        self.follow(Some(&edit.changes), Some(kind), Some((edit.selections, edit.main)), origin, cx);
        self.tell_other_editors(base, &edit.changes, cx);
        self.reveal_caret(cx);
        Ok(())
    }

    /// Applies the edit `make` describes for the current text and selection. An edit without
    /// changes only moves the selection. When the backend has moved on (its own edit not yet
    /// delivered here), the text is refreshed and `make` is asked again, so what the user typed
    /// lands at the caret's new place instead of being lost. Answers whether anything changed.
    pub(super) fn edit_with(
        &mut self,
        kind: EditKind,
        cx: &mut Context<Self>,
        make: impl Fn(&Self) -> Option<Edit>,
    ) -> bool {
        let origin = self.next_origin.take().unwrap_or(match kind {
            EditKind::Delete => EditOrigin::Deleted,
            _ => EditOrigin::Other,
        });
        for attempt in 0..3 {
            let Some(edit) = make(self) else { return false };
            if edit.changes.is_empty() {
                let moved = (&edit.selections, edit.main) != (&self.selections, self.main);
                self.set_selections(edit.selections, edit.main, cx);
                return moved;
            }
            if self.read_only {
                return false;
            }
            match self.send(edit, kind, origin.clone(), cx) {
                Ok(()) => return true,
                // The backend's edit becomes an undo entry of its own before the user's.
                Err(error) if error.kind == ErrorKind::OutOfSync && attempt < 2 => {
                    self.follow(None, Some(EditKind::Other), None, EditOrigin::External, cx);
                }
                Err(error) => {
                    tracing::warn!("edit of {} rejected: {}", self.path.display(), error.message);
                    return false;
                }
            }
        }
        false
    }

    /// Applies `changes` (in the coordinates of the current text, sorted and not overlapping) as
    /// one edit of the document and one undo step, after which `selections` (in the coordinates
    /// of the new text) is the selection, `main` its main range. A read-only editor refuses; so
    /// does the backend when the text moved on since the changes were computed (the editor then
    /// shows the new text). Answers whether the edit was applied.
    pub fn apply_edit(
        &mut self,
        changes: &[TextChange],
        selections: Vec<(u32, u32)>,
        main: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        self.apply_edit_from(EditOrigin::Other, changes, selections, main, cx)
    }

    /// [`Editor::apply_edit`], reported to the owner as an edit of `origin`.
    pub fn apply_edit_from(
        &mut self,
        origin: EditOrigin,
        changes: &[TextChange],
        selections: Vec<(u32, u32)>,
        main: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.read_only || changes.is_empty() {
            return false;
        }
        let edit = Edit { changes: changes.to_vec(), selections, main };
        match self.send(edit, EditKind::Other, origin, cx) {
            Ok(()) => true,
            Err(error) => {
                if error.kind == ErrorKind::OutOfSync {
                    self.follow(None, Some(EditKind::Other), None, EditOrigin::External, cx);
                } else {
                    tracing::warn!("edit of {} rejected: {}", self.path.display(), error.message);
                }
                false
            }
        }
    }

    /// Applies one side of history entries. On a rejection the history no longer describes the
    /// text: it is dropped and the text refreshed.
    fn replay(&mut self, steps: &[(Vec<TextChange>, history::Selection)], cx: &mut Context<Self>) -> bool {
        for (changes, selection) in steps {
            let applied =
                self.documents().and_then(|d| d.edit(self.doc, self.version, ChangeSet { changes: changes.clone() }));
            if let Err(error) = applied {
                tracing::warn!("cannot replay an edit of {}: {}", self.path.display(), error.message);
                self.history.clear();
                self.follow(None, None, None, EditOrigin::External, cx);
                return false;
            }
            self.dirty = true;
            let base = self.version;
            let selection = Some((selection.ranges.clone(), selection.main));
            self.follow(Some(changes), None, selection, EditOrigin::History, cx);
            self.tell_other_editors(base, changes, cx);
        }
        self.reveal_caret(cx);
        cx.notify();
        true
    }

    /// Undoes the newest group of edits; the selection returns to where it was before them.
    pub fn undo(&mut self, cx: &mut Context<Self>) -> bool {
        let entries = self.history.take_undo();
        if entries.is_empty() || self.read_only {
            return false;
        }
        let steps: Vec<_> = entries.iter().map(|e| (e.inverse.clone(), e.before.clone())).collect();
        let done = self.replay(&steps, cx);
        if done {
            self.history.undone(entries);
        }
        done
    }

    /// Redoes the group that was undone last.
    pub fn redo(&mut self, cx: &mut Context<Self>) -> bool {
        let entries = self.history.take_redo();
        if entries.is_empty() || self.read_only {
            return false;
        }
        let steps: Vec<_> = entries.iter().map(|e| (e.forward.clone(), e.after.clone())).collect();
        let done = self.replay(&steps, cx);
        if done {
            self.history.redone(entries);
        }
        done
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Changes the line separator the document is saved with; the document then has unsaved changes.
    pub fn set_line_ending(&mut self, line_ending: LineEnding, cx: &mut Context<Self>) -> IdeResult<()> {
        if line_ending == self.line_ending {
            return Ok(());
        }
        self.documents()?.set_line_ending(self.doc, line_ending)?;
        (self.line_ending, self.dirty) = (line_ending, true);
        cx.notify();
        Ok(())
    }

    /// Changes the document's encoding. With `reload` the file is read again from disk in that
    /// encoding, which the backend refuses while there are unsaved changes; without, the text
    /// stays and is written in the encoding on the next save.
    pub fn set_encoding(&mut self, encoding: &str, reload: bool, cx: &mut Context<Self>) -> IdeResult<()> {
        self.encoding = self.documents()?.set_encoding(self.doc, encoding, reload)?;
        if reload {
            // The text is another one: what could be undone no longer applies to it.
            self.history.clear();
            self.follow(None, None, None, EditOrigin::External, cx);
            self.dirty = false;
        } else {
            self.dirty = true;
        }
        cx.notify();
        Ok(())
    }

    /// Whether the document has changes that are not on disk.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Writes the document to disk; the backend reports the document clean when it is done.
    pub fn save(&self, cx: &App) -> impl Future<Output = anyhow::Result<IdeResult<()>>> + Send + 'static + use<> {
        let (host, window, doc) = (self.host.clone(), self.window_label.clone(), self.doc);
        cx.global::<Backend>().call(async move { ide_host::ops::doc::save(&host, &window, doc).await })
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.select(0, buffer::len_utf16(&self.rope), cx);
    }

    /// The text of the main selection.
    pub fn selected_text(&self) -> String {
        let selection = self.selection();
        buffer::slice(&self.rope, selection.start, selection.end)
    }

    /// Copies the selected ranges to the clipboard, joined by line breaks; without a selection,
    /// the caret lines (a line-wise copy, which a paste at a caret puts above its line).
    pub fn copy(&self, cx: &mut Context<Self>) -> bool {
        let copied = crate::editor_commands::clipboard::copy(&self.edit_state());
        cx.write_to_clipboard(ClipboardItem::new_string(copied.text.clone()));
        cx.default_global::<EditorClipboard>().0.record(&copied.text, copied.linewise);
        true
    }

    /// Copies as [`Editor::copy`] does and removes what was copied: the selected ranges, or the
    /// caret lines with their line breaks.
    pub fn cut(&mut self, cx: &mut Context<Self>) -> bool {
        self.copy(cx)
            && self.edit_with(EditKind::Other, cx, |editor| {
                let (_, edit) = crate::editor_commands::clipboard::cut(&editor.edit_state());
                Some(edit)
            })
    }

    /// Pastes the clipboard's text: a line-wise copy above the caret lines, one line per caret
    /// when the line count matches, reformatted as Smart Keys › Reformat on paste says.
    pub fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        match cx.read_from_clipboard().and_then(|item| item.text()) {
            Some(text) => self.paste_text(&text, true, cx),
            None => false,
        }
    }

    /// Pastes `text`; `reformat` applies Reformat on Paste.
    pub(super) fn paste_text(&mut self, text: &str, reformat: bool, cx: &mut Context<Self>) -> bool {
        use crate::editor_commands::{clipboard, paste};
        let linewise = cx.try_global::<EditorClipboard>().is_some_and(|history| history.0.is_linewise_text(text));
        self.edit_with(EditKind::Other, cx, |editor| {
            let state = editor.edit_state();
            let pasted = clipboard::paste(&state, text, linewise)?;
            if !reformat {
                return Some(pasted);
            }
            let indent = |context: &commands::IndentContext<'_>| editor.facts.indent(context);
            Some(paste::reindent_paste(&state, &pasted, editor.facts.reformat_on_paste, Some(&indent)))
        })
    }

    /// `text` with the document's line breaks.
    fn with_line_breaks(&self, text: &str) -> String {
        let unix = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.line_break() {
            "\n" => unix,
            line_break => unix.replace('\n', line_break),
        }
    }

    /// Replacing `range` (the selection ranges when `None`) with `text`, the caret after it. A
    /// range around the main selection, with several ranges, is replaced around each of them
    /// alike, as a composition at every caret.
    fn typed_edit(&self, range: Option<Range<u32>>, text: &str) -> Option<Edit> {
        let state = self.edit_state();
        let length = commands::utf16_len(text);
        let main = self.main_range();
        let around = range.clone().map_or(Some((0, 0)), |range| {
            (range.start <= main.start() && range.end >= main.end())
                .then(|| (main.start() - range.start, range.end - main.end()))
        });
        let ranges = around.and_then(|(before, after)| {
            let len = buffer::len_utf16(&self.rope);
            let spans: Vec<(u32, u32)> = self
                .selections
                .iter()
                .map(|range| (range.start().saturating_sub(before), (range.end() + after).min(len)))
                .collect();
            spans.windows(2).all(|pair| pair[0].1 <= pair[1].0).then_some(spans)
        });
        let edit = match ranges {
            Some(spans) => change_by_range(&state, |index, _| {
                let (from, to) = spans[index];
                RangeEdit { changes: vec![change(from, to, text)], range: caret(from + length) }
            }),
            None => {
                let range = range?;
                let changes = vec![change(range.start, range.end, text)];
                let mut edit = Edit::following(&state, changes, Assoc::After);
                edit.selections[edit.main] = caret(range.start + length);
                Edit::new(edit.changes, edit.selections, edit.main)
            }
        };
        let empty = edit.changes.iter().all(|change| change.from == change.to && change.insert.is_empty());
        (!empty).then_some(edit)
    }

    /// Replaces `range` (every selection range when `None`) with typed `text`; the carets end
    /// after it.
    pub fn insert(&mut self, range: Option<Range<u32>>, text: &str, cx: &mut Context<Self>) -> bool {
        let text = self.with_line_breaks(text);
        self.next_origin = Some(EditOrigin::Typed(text.clone()));
        self.edit_with(EditKind::Insert, cx, |editor| editor.typed_edit(range.clone(), &text))
    }

    /// Typing `text` at every caret: over the closing quote of a string literal the caret steps
    /// past it, in overwrite mode the text replaces what follows the caret.
    fn type_text(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if self.read_only {
            return false;
        }
        let text = self.with_line_breaks(text);
        self.next_origin = Some(EditOrigin::Typed(text.clone()));
        self.edit_with(EditKind::Insert, cx, |editor| {
            let state = editor.edit_state();
            if let Some(edit) = smart_keys::skip_closing_quote(&state, &editor.literal_tree(), &text) {
                return Some(edit);
            }
            if editor.overwrite
                && let Some(edit) = crate::editor_commands::overwrite::type_overwriting(&state, &text)
            {
                return Some(edit);
            }
            editor.typed_edit(None, &text)
        })
    }

    /// Enter: a documentation comment stub after `/**`, the rest of a line comment on a new
    /// commented line, else a line break with the indentation the language computes.
    fn enter(&mut self, cx: &mut Context<Self>) -> bool {
        use crate::editor_commands::{comments, doc_comment, indent};
        self.next_origin = Some(EditOrigin::Typed(self.line_break().to_owned()));
        self.edit_with(EditKind::Insert, cx, |editor| {
            let state = editor.edit_state();
            if editor.facts.insert_doc_stub
                && let Some(edit) = doc_comment::enter_in_doc_comment(&state, &editor.facts.language)
            {
                return Some(edit);
            }
            let comments_at = |pos: u32| editor.facts.comments_at(pos);
            if let Some(edit) = comments::enter_in_line_comment(&state, &comments_at, &editor.literal_tree()) {
                return Some(edit);
            }
            let indent = |context: &commands::IndentContext<'_>| editor.facts.indent(context);
            indent::insert_newline_and_indent(&state, Some(&indent))
        })
    }

    /// Tab: out of a pair the editor inserted, else indentation of the selected lines or
    /// whitespace to the next indent stop.
    fn tab(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some((edit, remaining)) = smart_keys::tab_out_of_scope(&self.edit_state(), &self.pair_scopes) {
            self.set_selections(edit.selections, edit.main, cx);
            self.pair_scopes = remaining;
            return true;
        }
        self.edit_with(EditKind::Insert, cx, |editor| Some(smart_keys::insert_tab(&editor.edit_state(), &[]).0))
    }

    /// Backspace: in leading whitespace to the proper indentation (Smart Keys › Backspace ›
    /// Unindent), else the character before every caret, or the selected ranges.
    fn backspace(&mut self, cx: &mut Context<Self>) -> bool {
        self.edit_with(EditKind::Delete, cx, |editor| {
            if smart_keys::smart_backspace_applies(&editor.facts.backspace_unindent, &editor.facts.language.id) {
                let indent = |context: &commands::IndentContext<'_>| editor.facts.indent(context);
                if let Some(edit) = smart_keys::smart_backspace(&editor.edit_state(), Some(&indent)) {
                    return Some(edit);
                }
            }
            editor.deletion(|editor, head| editor.previous_offset(head))
        })
    }

    /// Deletes the selected ranges, or the character before (`forward` false) or after every
    /// caret.
    pub fn delete(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        self.edit_with(EditKind::Delete, cx, |editor| {
            editor
                .deletion(|editor, head| if forward { editor.next_offset(head) } else { editor.previous_offset(head) })
        })
    }

    /// Deletes the selected ranges, and at every caret the text between it and `other(head)`.
    fn deletion(&self, other: impl Fn(&Self, u32) -> u32) -> Option<Edit> {
        let edit = change_by_range(&self.edit_state(), |_, range| {
            let (from, to) = if range.is_caret() {
                let other = other(self, range.head());
                (other.min(range.head()), other.max(range.head()))
            } else {
                (range.start(), range.end())
            };
            let changes = if from < to { vec![change(from, to, "")] } else { Vec::new() };
            RangeEdit { changes, range: caret(from) }
        });
        (!edit.changes.is_empty()).then_some(edit)
    }

    /// Deletes by word before (`forward` false) or after every caret; by hump with CamelHumps.
    fn delete_word(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        self.edit_with(EditKind::Delete, cx, |editor| {
            if editor.facts.camel_humps {
                let edit = editing::delete_hump(&editor.edit_state(), forward, true);
                return (!edit.changes.is_empty()).then_some(edit);
            }
            editor.deletion(|editor, head| buffer::group_boundary(&editor.rope, head, forward))
        })
    }

    /// Moves every caret by word; by hump with CamelHumps.
    fn move_by_word(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
        if self.facts.camel_humps {
            let edit = editing::move_hump(&self.edit_state(), forward, extend);
            self.set_selections(edit.selections, edit.main, cx);
        } else {
            self.move_carets(extend, cx, |editor, range| buffer::group_boundary(&editor.rope, range.head(), forward));
        }
    }

    /// Escape: what the editor's tab shows over it closes first (its Find bar); else several
    /// ranges become the main one, a selection its caret. Answers whether Escape was used.
    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(handler) = self.escape.clone()
            && handler(window, cx)
        {
            return true;
        }
        let main = self.main_range();
        if self.selections.len() > 1 {
            self.set_selections(vec![main], 0, cx);
        } else if !main.is_caret() {
            self.select(main.head(), main.head(), cx);
        } else {
            return false;
        }
        true
    }

    /// The offset one character (not code unit) after `offset`; a line break counts as one.
    fn next_offset(&self, offset: u32) -> u32 {
        let char = self.rope.utf16_cu_to_char(offset as usize);
        if char >= self.rope.len_chars() {
            return offset;
        }
        let step =
            if self.rope.char(char) == '\r' && char + 1 < self.rope.len_chars() && self.rope.char(char + 1) == '\n' {
                2
            } else {
                1
            };
        self.rope.char_to_utf16_cu(char + step) as u32
    }

    fn previous_offset(&self, offset: u32) -> u32 {
        let char = self.rope.utf16_cu_to_char(offset as usize);
        if char == 0 {
            return 0;
        }
        let step =
            if char >= 2 && self.rope.char(char - 1) == '\n' && self.rope.char(char - 2) == '\r' { 2 } else { 1 };
        self.rope.char_to_utf16_cu(char - step) as u32
    }

    fn refresh_highlights(&mut self, cx: &mut Context<Self>) {
        if self.large {
            self.highlights.clear();
            return;
        }
        super::core_actions::refresh_folds(self, cx);
        let (host, window, doc) = (self.host.clone(), self.window_label.clone(), self.doc);
        let generation = self.generation;
        cx.spawn(async move |editor, cx| {
            // Highlighting reads the whole document: while the user types, only the last edit
            // of a burst asks for it.
            cx.background_executor().timer(HIGHLIGHT_DELAY).await;
            let request = editor.update(cx, |editor, cx| {
                (editor.generation == generation).then(|| {
                    let work = async move { ide_host::ops::doc::doc_highlights(&host, &window, doc).await };
                    (editor.version, cx.global::<Backend>().call(work))
                })
            });
            let Ok(Some((version, work))) = request else { return };
            let Ok(Ok(highlights)) = work.await else { return };
            let _ = editor.update(cx, |editor: &mut Editor, cx| {
                // Highlights of a version the text has already left would colour the wrong ranges.
                if editor.version != version {
                    return;
                }
                let (triples, _) = highlights.data.as_chunks::<3>();
                editor.highlights = triples
                    .iter()
                    .filter_map(|&[from, to, kind]| {
                        Some(HighlightSpan { from, to, kind: *highlights.kinds.get(kind as usize)? })
                    })
                    .collect();
                editor.highlights_version = version;
                cx.notify();
            });
        })
        .detach();
    }

    /// The pixel position (top left of the caret) of `offset`, when its line was painted.
    pub fn position_of(&self, offset: u32) -> Option<Point<Pixels>> {
        let layout = self.layout.as_ref()?;
        let (line, col) = buffer::line_col(&self.rope, offset);
        let row = self.core.folds.row_of_line(line);
        let index = row.checked_sub(layout.first_line)? as usize;
        // A hidden line is not painted.
        if layout.line_of.get(index) != Some(&line) {
            return None;
        }
        let (text, shaped) = layout.lines.get(index)?;
        let x = shaped.x_for_index(buffer::byte_of_col(text, col));
        let y = layout.line_height * row as f32 - self.scroll_top;
        Some(point(layout.bounds.left() + layout.text_left + x, layout.bounds.top() + y))
    }

    /// The offset nearest to a point of the window.
    pub fn offset_at(&self, position: Point<Pixels>) -> Option<u32> {
        let layout = self.layout.as_ref()?;
        let y = (position.y - layout.bounds.top() + self.scroll_top).max(px(0.0));
        let rows = self.core.folds.row_count(self.line_count());
        let row = ((y / layout.line_height) as u32).min(rows.saturating_sub(1));
        let line = self.core.folds.line_of_row(row);
        let x = position.x - layout.bounds.left() - layout.text_left;
        let col = match layout.lines.get(row.checked_sub(layout.first_line)? as usize) {
            Some((text, shaped)) => buffer::col_of_byte(text, shaped.closest_index_for_x(x)),
            None => 0,
        };
        Some(buffer::offset_at(&self.rope, line, col))
    }

    /// ↑ / ↓ by `lines` (negative up), every caret keeping its goal column; with `extend` the
    /// heads move and the anchors stay.
    fn vertical(&mut self, lines: i64, extend: bool, cx: &mut Context<Self>) {
        use vertical_motion::{cursor_line_down, cursor_line_up, select_line_down, select_line_up};
        let step = match (lines > 0, extend) {
            (false, false) => cursor_line_up,
            (true, false) => cursor_line_down,
            (false, true) => select_line_up,
            (true, true) => select_line_down,
        };
        let mut goals = recall(&self.memory.goals, &self.stamp()).unwrap_or_default();
        let mut state = self.edit_state();
        let mut moved = false;
        for _ in 0..lines.unsigned_abs() {
            let Some(motion) = step(&state, &goals) else { break };
            state = commands::apply(&state, &motion.edit);
            goals = motion.goals;
            moved = true;
            // A line inside a fold is not on screen: the caret goes on to the next line shown.
            while self.core.folds.is_hidden(state.line_at(state.main().head()).number) {
                let before = state.main().head();
                let Some(motion) = step(&state, &goals) else { break };
                state = commands::apply(&state, &motion.edit);
                goals = motion.goals;
                if state.main().head() == before {
                    break;
                }
            }
        }
        if moved {
            self.set_selections(state.selections, state.main, cx);
            self.memory.goals = Some((self.stamp(), goals));
        }
    }

    /// ⇧+arrow in Column Selection Mode: the block grows by `lines` lines and `cols` columns.
    /// Answers false while the mode is off.
    fn extend_column(&mut self, lines: i32, cols: i32, cx: &mut Context<Self>) -> bool {
        let block = self.memory.column_block.as_ref().filter(|(stamp, _)| *stamp == self.stamp()).map(|(_, b)| *b);
        let mode = commands::column_mode::ColumnMode { on: self.memory.column_on, block };
        let Some((edit, block)) = commands::column_mode::extend_block(&self.edit_state(), &mode, lines, cols) else {
            return false;
        };
        self.set_selections(edit.selections, edit.main, cx);
        self.memory.column_block = Some((self.stamp(), block));
        true
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        // The editing shortcuts every text field has (⌘Z, ⌘C…; Ctrl on Windows and Linux).
        if modifiers.secondary(cx) && !modifiers.alt && modifiers.number_of_modifiers() <= 2 {
            let handled = match (keystroke.key.as_str(), modifiers.shift) {
                ("z", false) => Some(self.undo(cx)),
                ("z", true) => Some(self.redo(cx)),
                ("c", false) => Some(self.copy(cx)),
                ("x", false) => Some(self.cut(cx)),
                ("v", false) => Some(self.paste(cx)),
                ("a", false) => {
                    self.select_all(cx);
                    Some(true)
                }
                _ => None,
            };
            if handled.is_some() {
                window.invalidate_character_coordinates();
                cx.stop_propagation();
                return;
            }
        }
        let extend = modifiers.shift;
        let line_start = |editor: &Self, range: Selection| {
            buffer::line_range(&editor.rope, buffer::line_col(&editor.rope, range.head()).0).start
        };
        let line_end = |editor: &Self, range: Selection| {
            buffer::line_range(&editor.rope, buffer::line_col(&editor.rope, range.head()).0).end
        };
        // Movement and deletion by word (⌥) and to the line's or document's edge (⌘), as
        // CodeMirror's standard keymap binds them on macOS.
        if (modifiers.alt || modifiers.platform) && !modifiers.control && !(modifiers.alt && modifiers.platform) {
            let end = buffer::len_utf16(&self.rope);
            match (keystroke.key.as_str(), modifiers.alt) {
                ("left", true) => self.move_by_word(false, extend, cx),
                ("right", true) => self.move_by_word(true, extend, cx),
                ("left", false) => self.move_carets(extend, cx, line_start),
                ("right", false) => self.move_carets(extend, cx, line_end),
                ("up", false) => self.move_carets(extend, cx, |_, _| 0),
                ("down", false) => self.move_carets(extend, cx, |_, _| end),
                ("backspace", true) if !extend => {
                    self.delete_word(false, cx);
                }
                ("backspace", false) if !extend => {
                    self.edit_with(EditKind::Delete, cx, |editor| {
                        editor.deletion(|editor, head| line_start(editor, caret(head)))
                    });
                }
                ("delete", true) if !extend => {
                    self.delete_word(true, cx);
                }
                _ => return,
            }
            window.invalidate_character_coordinates();
            cx.stop_propagation();
            return;
        }
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let column_step = match keystroke.key.as_str() {
            "left" => Some((0, -1)),
            "right" => Some((0, 1)),
            "up" => Some((-1, 0)),
            "down" => Some((1, 0)),
            _ => None,
        };
        if let Some((lines, cols)) = column_step.filter(|_| extend)
            && self.extend_column(lines, cols, cx)
        {
            window.invalidate_character_coordinates();
            cx.stop_propagation();
            return;
        }
        match keystroke.key.as_str() {
            "left" => self.move_carets(extend, cx, |editor, range| {
                if !extend && !range.is_caret() {
                    range.start()
                } else {
                    editor.core.folds.step(range.head(), editor.previous_offset(range.head()))
                }
            }),
            "right" => self.move_carets(extend, cx, |editor, range| {
                if !extend && !range.is_caret() {
                    range.end()
                } else {
                    editor.core.folds.step(range.head(), editor.next_offset(range.head()))
                }
            }),
            "up" => self.vertical(-1, extend, cx),
            "down" => self.vertical(1, extend, cx),
            "home" => self.move_carets(extend, cx, line_start),
            "end" => self.move_carets(extend, cx, line_end),
            "pageup" => self.vertical(-self.page_lines(cx), extend, cx),
            "pagedown" => self.vertical(self.page_lines(cx), extend, cx),
            "backspace" => {
                self.backspace(cx);
            }
            "delete" => {
                self.delete(true, cx);
            }
            "enter" => {
                self.enter(cx);
            }
            "tab" if !extend => {
                self.tab(cx);
            }
            "escape" => {
                if !self.escape(window, cx) {
                    return;
                }
            }
            _ => return,
        }
        window.invalidate_character_coordinates();
        cx.stop_propagation();
    }

    /// Lines a Page Up / Page Down moves by: one viewport less a line of context.
    fn page_lines(&self, cx: &App) -> i64 {
        let viewport = self.layout.as_ref().map_or(px(0.0), |l| l.bounds.size.height);
        (((viewport / self.metrics(cx).1).floor() as i64) - 1).max(1)
    }

    /// The press is the keymap's mouse shortcut for adding or removing a caret.
    fn adds_caret(event: &MouseDownEvent, cx: &App) -> bool {
        let Some(keymap) = cx.try_global::<input::Keymap>() else { return false };
        let Some(stroke) = crate::platform::dispatcher::mouse_stroke(0, input::mods(&event.modifiers)) else {
            return false;
        };
        keymap.0.bindings_for(ADD_CARET_ACTION).iter().any(|binding| normalize_binding(binding) == stroke)
    }

    /// A click places the caret (Shift extends), a double click selects the word, a triple
    /// click the line; dragging extends from where the press began. The add-caret shortcut
    /// adds a caret where it is pressed (a range, dragging), or removes the caret there.
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if let Some(line) = self.gutter_line_at(event.position) {
            (self.drag_anchor, self.adding) = (None, None);
            // The chevrons' column opens and closes the line's fold and takes no breakpoint.
            if self.in_fold_column(event.position) {
                self.toggle_fold_at_line(line, cx);
                return;
            }
            cx.emit(EditorEvent::GutterPressed { line, shift: event.modifiers.shift, alt: event.modifiers.alt });
            return;
        }
        if let Some(handler) = self.pointer.clone()
            && handler(event, self.offset_at(event.position), window, cx)
        {
            (self.drag_anchor, self.adding) = (None, None);
            return;
        }
        let Some(offset) = self.offset_at(event.position) else { return };
        if event.click_count <= 1 && Self::adds_caret(event, cx) {
            let (ranges, main) = (self.selections.clone(), self.main);
            let (next, next_main) = editing::toggle_caret(&ranges, main, offset, offset);
            self.set_selections(next, next_main, cx);
            self.adding = Some(AddingCaret { ranges, main, anchor: offset });
            self.drag_anchor = None;
            return;
        }
        self.adding = None;
        self.column_drag = None;
        // ⌥-drag selects a block of columns (the add-caret gesture, checked above, comes first).
        let modifiers = &event.modifiers;
        if event.click_count <= 1 && modifiers.alt && !modifiers.shift && !modifiers.platform && !modifiers.control {
            self.select(offset, offset, cx);
            self.column_drag = self.block_point_at(event.position);
            self.drag_anchor = None;
            return;
        }
        match event.click_count {
            0 | 1 => {
                let anchor = if event.modifiers.shift { self.anchor() } else { offset };
                self.select(anchor, offset, cx);
            }
            2 => {
                let word = buffer::word_at(&self.rope, offset);
                self.select(word.start, word.end, cx);
            }
            _ => {
                let (line, _) = buffer::line_col(&self.rope, offset);
                let range = buffer::line_range(&self.rope, line);
                // The whole line with its break, so that typing replaces the line; the lines a fold
                // hides go with their header.
                let next = self.core.folds.line_of_row(self.core.folds.row_of_line(line) + 1);
                let end = if next < self.line_count() { buffer::line_range(&self.rope, next).start } else { range.end };
                self.select(range.start, end, cx);
            }
        }
        self.drag_anchor = Some(self.anchor());
    }

    /// Whether `position` is on the column of the fold chevrons, the gutter's last before the text.
    fn in_fold_column(&self, position: Point<Pixels>) -> bool {
        self.layout.as_ref().is_some_and(|layout| {
            let x = position.x - layout.bounds.left();
            x >= layout.gutter - layout.fold_column && x < layout.gutter
        })
    }

    /// The 0-based line whose gutter (left of the text) is under `position`.
    fn gutter_line_at(&self, position: Point<Pixels>) -> Option<u32> {
        let layout = self.layout.as_ref()?;
        let x = position.x - layout.bounds.left();
        let y = position.y - layout.bounds.top() + self.scroll_top;
        if x < px(0.0) || x >= layout.gutter || y < px(0.0) {
            return None;
        }
        let line = self.core.folds.line_of_row((y / layout.line_height) as u32);
        (line < self.line_count()).then_some(line)
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            (self.drag_anchor, self.adding, self.column_drag) = (None, None, None);
            let over_gutter = self.gutter_line_at(event.position).is_some();
            if over_gutter != self.gutter_hovered {
                self.gutter_hovered = over_gutter;
                cx.notify();
            }
            if event.pressed_button.is_none() {
                self.hover(self.text_offset_at(event.position), event.modifiers, cx);
            }
            return;
        }
        let Some(offset) = self.offset_at(event.position) else { return };
        if let Some(adding) = &self.adding {
            if offset != adding.anchor {
                let (next, main) = editing::toggle_caret(&adding.ranges, adding.main, adding.anchor, offset);
                self.set_selections(next, main, cx);
            }
            return;
        }
        if let Some(start) = self.column_drag {
            let Some(head) = self.block_point_at(event.position) else { return };
            let mut ranges = commands::column_mode::rectangle_ranges(&self.edit_state(), start, head);
            // The block covers the rows on screen: a range on a line a fold hides would open the fold.
            ranges.retain(|range| !self.core.folds.is_hidden(buffer::line_col(&self.rope, range.head()).0));
            if !ranges.is_empty() && ranges != self.selections {
                let main = ranges.len() - 1;
                self.set_selections(ranges, main, cx);
            }
            return;
        }
        let Some(anchor) = self.drag_anchor else { return };
        if offset != self.caret() {
            self.select(anchor, offset, cx);
        }
    }

    /// The line and visible column under a point, for a block selection: right of a line's end the
    /// column goes on growing by the width of a column of the painted text.
    fn block_point_at(&self, position: Point<Pixels>) -> Option<commands::column_mode::BlockPoint> {
        let offset = self.offset_at(position)?;
        let (line, col) = buffer::line_col(&self.rope, offset);
        let tab_size = self.facts.tab_size;
        let text = self.edit_state().line(line).text;
        let mut column = commands::visible_column::visible_column(&text, col, tab_size);
        let layout = self.layout.as_ref()?;
        let column_width = layout.lines.iter().find_map(|(text, shaped)| {
            let columns = commands::visible_column::visible_width(text, tab_size);
            (columns > 0).then(|| shaped.width / columns as f32)
        });
        let painted = layout.line_of.iter().position(|shown| *shown == line).and_then(|index| layout.lines.get(index));
        if let (Some(width), Some((_, shaped))) = (column_width, painted)
            && col as usize == text.encode_utf16().count()
        {
            let beyond = position.x - layout.bounds.left() - layout.text_left - shaped.width;
            if beyond > px(0.0) {
                column += (beyond / width).round() as u32;
            }
        }
        Some(commands::column_mode::BlockPoint { line, col: column })
    }
}

impl Editor {
    /// The offset of the character under a point of the window: `None` beside the text (past
    /// the end of its line, or outside the painted lines).
    pub fn text_offset_at(&self, position: Point<Pixels>) -> Option<u32> {
        let layout = self.layout.as_ref()?;
        let y = position.y - layout.bounds.top() + self.scroll_top;
        if y < px(0.0) {
            return None;
        }
        let row = (y / layout.line_height) as u32;
        let index = row.checked_sub(layout.first_line)? as usize;
        let (text, shaped) = layout.lines.get(index)?;
        let line = *layout.line_of.get(index)?;
        let x = position.x - layout.bounds.left() - layout.text_left;
        if x < px(0.0) || x >= shaped.width {
            return None;
        }
        let byte = shaped.index_for_x(x)?;
        Some(buffer::offset_at(&self.rope, line, buffer::col_of_byte(text, byte)))
    }

    /// Reports where the pointer rests, when that changed.
    fn hover(&mut self, offset: Option<u32>, modifiers: Modifiers, cx: &mut Context<Self>) {
        if self.hovered != (offset, modifiers) {
            self.hovered = (offset, modifiers);
            cx.emit(EditorEvent::Hovered { offset, modifiers });
        }
    }
}

/// One change turning `old` into `new`: what lies between their common prefix and suffix.
fn difference(old: &Rope, new: &Rope) -> Vec<TextChange> {
    let prefix = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).count();
    let (old_rest, new_rest) = (old.len_chars() - prefix, new.len_chars() - prefix);
    let mut suffix = 0;
    {
        let (mut a, mut b) = (old.chars_at(old.len_chars()), new.chars_at(new.len_chars()));
        while suffix < old_rest.min(new_rest) && a.prev() == b.prev() {
            suffix += 1;
        }
    }
    let (from, to) = (prefix, old.len_chars() - suffix);
    let insert = new.slice(prefix..new.len_chars() - suffix).to_string();
    if from == to && insert.is_empty() {
        return Vec::new();
    }
    vec![TextChange { from: old.char_to_utf16_cu(from) as u32, to: old.char_to_utf16_cu(to) as u32, insert }]
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let len = buffer::len_utf16(&self.rope) as usize;
        let clamped = range.start.min(len)..range.end.min(len);
        if clamped != range {
            *adjusted = Some(clamped.clone());
        }
        Some(buffer::slice(&self.rope, clamped.start as u32, clamped.end as u32))
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let selection = self.selection();
        let main = self.main_range();
        Some(UTF16Selection {
            range: selection.start as usize..selection.end as usize,
            reversed: main.head() < main.anchor(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|m| m.start as usize..m.end as usize)
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Committed text replaces the composition, else the given range, else every selection
        // range as typing does.
        match range.map(|r| r.start as u32..r.end as u32).or_else(|| self.marked.clone()) {
            Some(range) => {
                self.insert(Some(range), text, cx);
            }
            None => {
                self.type_text(text, cx);
            }
        }
        self.marked = None;
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| r.start as u32..r.end as u32)
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection());
        let inserted = self.insert(Some(range.clone()), new_text, cx);
        if !inserted && !new_text.is_empty() {
            return;
        }
        let length = new_text.encode_utf16().count() as u32;
        // The main caret ends after the composed text, wherever the other carets' copies moved it.
        let start = if inserted { self.caret().saturating_sub(length) } else { range.start };
        self.marked = (length > 0).then_some(start..start + length);
        // The composition's own selection is relative to the composed text.
        let selected = new_selected.map_or(length..length, |r| r.start as u32..r.end as u32);
        let mut ranges = self.selections.clone();
        if let Some(main) = ranges.get_mut(self.main) {
            *main = (start + selected.start.min(length), start + selected.end.min(length));
        }
        self.put_selections(ranges, self.main);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let from = self.position_of(range.start as u32)?;
        let to = self.position_of(range.end as u32).filter(|to| to.y == from.y).unwrap_or(from);
        Some(Bounds::new(from, size((to.x - from.x).max(px(1.0)), layout.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        self.offset_at(point).map(|offset| offset as usize)
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("editor")
            .size_full()
            .key_context(EDITOR_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|editor, _, _, _| (editor.drag_anchor, editor.adding) = (None, None)),
            )
            .on_hover(cx.listener(|editor, hovered: &bool, _, cx| {
                if !hovered {
                    editor.hover(None, Modifiers::default(), cx);
                }
            }))
            .child(EditorElement { editor: cx.entity() })
            .children(self.overlay.clone().map(|overlay| div().absolute().top_0().left_0().size_0().child(overlay)))
    }
}

/// Paints the visible part of an [`Editor`].
struct EditorElement {
    editor: Entity<Editor>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

struct Prepared {
    layout: Layout,
    numbers: Vec<ShapedLine>,
    gutter: Pixels,
    /// The marks that touch the painted lines.
    marks: Vec<Mark>,
    /// Per painted row: the placeholder of the fold its line starts, and the text after the fold.
    placeholders: Vec<Option<(ShapedLine, ShapedLine)>>,
    /// Per painted row: the chevron of a folded line in the gutter.
    markers: Vec<Option<ShapedLine>>,
}

/// `runs` (covering a line's bytes in order) with the bytes `from..to` in `color`.
fn recolor(runs: Vec<TextRun>, from: usize, to: usize, color: Hsla) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for run in runs {
        let (start, end) = (at, at + run.len);
        at = end;
        let (cut_from, cut_to) = (from.clamp(start, end), to.clamp(start, end));
        if cut_from >= cut_to {
            out.push(run);
            continue;
        }
        for (a, b, recolored) in [(start, cut_from, false), (cut_from, cut_to, true), (cut_to, end, false)] {
            if b > a {
                out.push(TextRun { len: b - a, color: if recolored { color } else { run.color }, ..run.clone() });
            }
        }
    }
    out
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepared;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepared {
        let editor = self.editor.read(cx);
        let (font_size, line_height, gutter) = editor.metrics(cx);
        let fold_column = editor.fold_column_width(cx);
        let code_font = editor.code_font(cx);
        let brackets = editor.facts.rainbow_brackets.then(|| editor.bracket_marks());
        let plain = theme::color(cx, "fg-editor");
        let number_color = theme::color(cx, "fg-gutter");
        let match_color = theme::color(cx, "fg-search-match");
        let placeholder_color = theme::color(cx, "fg-fold-placeholder");
        let marker_color = theme::color(cx, "fg-fold-marker");
        let total = editor.line_count();
        // The frame paints the rows in view; a row is a line no fold hides.
        let folds = &editor.core.folds;
        let rows = folds.row_count(total);
        let first_row = ((editor.scroll_top / line_height).floor() as u32).min(rows.saturating_sub(1));
        let visible = (bounds.size.height / line_height).ceil() as u32 + 1;
        let last_row = (first_row + visible).min(rows);
        let painted: Vec<u32> =
            (first_row..last_row).map(|row| folds.line_of_row(row)).filter(|line| *line < total).collect();
        let first_line = painted.first().copied().unwrap_or(0);
        let last_line = painted.last().map_or(0, |line| line + 1);
        // A folded header's placeholder reads the text and its comments; with nothing folded
        // neither is needed.
        let state = (!folds.folded().is_empty()).then(|| editor.edit_state());
        let run = |len: usize, color: Hsla| TextRun {
            len,
            font: code_font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // The marks that can touch the painted lines.
        let first_offset = buffer::line_range(&editor.rope, first_line).start;
        let last_offset = buffer::line_range(&editor.rope, last_line.saturating_sub(1)).end;
        let marks: Vec<Mark> = editor
            .marks
            .values()
            .flatten()
            .filter(|mark| mark.range.end >= first_offset && mark.range.start <= last_offset)
            .cloned()
            .collect();
        let mut lines = Vec::with_capacity(painted.len());
        let mut numbers = Vec::with_capacity(painted.len());
        let mut placeholders = Vec::with_capacity(painted.len());
        let mut markers = Vec::with_capacity(painted.len());
        #[cfg(test)]
        let mut painted_colors = Vec::with_capacity(painted.len());
        for &line in &painted {
            let range = buffer::line_range(&editor.rope, line);
            let text = buffer::slice(&editor.rope, range.start, range.end);
            let mut runs = Vec::new();
            let mut at = 0usize;
            // Spans are sorted; the lines between two rows can be hidden, so each row looks its
            // first span up.
            let mut span = editor.highlights.partition_point(|s| s.to <= range.start);
            while let Some(s) = editor.highlights.get(span).filter(|s| s.from < range.end) {
                let from = buffer::byte_of_col(&text, s.from.saturating_sub(range.start)).max(at);
                let to = buffer::byte_of_col(&text, s.to.min(range.end) - range.start);
                if from > at {
                    runs.push(run(from - at, plain));
                }
                if to > from {
                    runs.push(run(to - from, theme::color(cx, syntax_token(s.kind))));
                    at = to;
                }
                if s.to > range.end {
                    break; // continues on the next line
                }
                span += 1;
            }
            if at < text.len() || runs.is_empty() {
                runs.push(run(text.len() - at, plain));
            }
            // Rainbow brackets: over the syntax colour, by depth in the scheme's three colours, the
            // unmatched ones in the error colour.
            if let Some(brackets) = &brackets {
                let first = brackets.partition_point(|mark| mark.at < range.start);
                for mark in brackets[first..].iter().take_while(|mark| mark.at < range.end) {
                    let from = buffer::byte_of_col(&text, mark.at - range.start);
                    let color = match mark.depth {
                        Some(depth) => theme::color(cx, BRACKET_COLORS[depth as usize % BRACKET_COLORS.len()]),
                        None => theme::color(cx, "syn-invalid"),
                    };
                    runs = recolor(runs, from, from + 1, color);
                }
            }
            for mark in marks.iter().filter(|mark| mark.kind.recolors_text()) {
                if mark.range.end > range.start && mark.range.start < range.end {
                    let from = buffer::byte_of_col(&text, mark.range.start.saturating_sub(range.start));
                    let to = buffer::byte_of_col(&text, mark.range.end.min(range.end) - range.start);
                    runs = recolor(runs, from, to, match_color);
                }
            }
            #[cfg(test)]
            painted_colors.push(runs.iter().map(|run| (run.len, run.color)).collect());
            let shaped = window.text_system().shape_line(text.clone().into(), font_size, &runs, None);
            lines.push((text, shaped));
            let label = (line + 1).to_string();
            numbers.push(window.text_system().shape_line(
                label.clone().into(),
                font_size,
                &[run(label.len(), number_color)],
                None,
            ));
            // A folded header shows a placeholder (a custom region's name), then what of the fold's
            // last line comes after it; its gutter shows the closed fold's chevron.
            let folded = state.as_ref().and_then(|state| folds.folded_on(state, line).map(|region| (state, region)));
            placeholders.push(folded.map(|(state, (from, to))| {
                let comments = |pos: u32| editor.facts().comments_at(pos);
                let name = crate::editor_commands::regions::region_name_at(state, &comments, from)
                    .unwrap_or_else(|| super::folds::PLACEHOLDER.to_owned());
                let tail_end = buffer::line_range(&editor.rope, buffer::line_col(&editor.rope, to).0).end;
                let tail = buffer::slice(&editor.rope, to, tail_end);
                let shape = |text: String, color: Hsla| {
                    let len = text.len();
                    window.text_system().shape_line(text.into(), font_size, &[run(len, color)], None)
                };
                (shape(name, placeholder_color), shape(tail, plain))
            }));
            // A folded header shows the closed chevron; a line that starts an open fold shows the
            // open one while the pointer is over the gutter.
            let open = editor.gutter_hovered && folds.is_foldable(line);
            let chevron = if folded.is_some() { Some("›") } else { open.then_some("⌄") };
            markers.push(chevron.map(|chevron| {
                window.text_system().shape_line(chevron.into(), font_size, &[run(chevron.len(), marker_color)], None)
            }));
        }
        // Distraction Free Mode centres a column of its width in characters (and the caret's room).
        let text_left = if distraction_free(cx) {
            let zero = window.text_system().shape_line("0".into(), font_size, &[run(1, plain)], None);
            let column = zero.width * editor.facts.distraction_free_width.clamp(20, 500) as f32 + px(22.0);
            ((bounds.size.width - column) / 2.0).max(px(0.0))
        } else {
            gutter
        };
        Prepared {
            layout: Layout {
                bounds,
                gutter,
                fold_column,
                text_left,
                line_height,
                first_line: first_row,
                lines,
                line_of: painted,
                #[cfg(test)]
                painted_marks: Vec::new(),
                #[cfg(test)]
                painted_colors,
            },
            numbers,
            gutter,
            marks,
            placeholders,
            markers,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepared: &mut Prepared,
        window: &mut Window,
        cx: &mut App,
    ) {
        use super::marks::MarkKind;
        let (focus, selections, scroll_top, marked) = {
            let editor = self.editor.read(cx);
            (editor.focus.clone(), editor.selections.clone(), editor.scroll_top, editor.marked.clone())
        };
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.editor.clone()), cx);
        let editor = self.editor.clone();
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && bounds.contains(&event.position) {
                let line_height = editor.read(cx).metrics(cx).1;
                let delta = event.delta.pixel_delta(line_height).y;
                editor.update(cx, |editor, cx| editor.scroll_by(-delta, cx));
                window.refresh();
            }
        });

        let Prepared { layout, numbers, gutter, marks, placeholders, markers } = prepared;
        let (line_height, text_left, fold_column) = (layout.line_height, layout.text_left, layout.fold_column);
        let focused = focus.is_focused(window);
        let mark_color = |cx: &App, kind: MarkKind| match kind {
            MarkKind::FindOccurrence => theme::color(cx, "bg-search-match"),
            MarkKind::CurrentFindOccurrence => theme::color(cx, "bg-search-match-current"),
            MarkKind::FindScope => theme::color(cx, "bg-editor-selection-inactive"),
            MarkKind::ExcludedOccurrence => theme::color(cx, "fg-muted"),
            MarkKind::ErrorProblem => theme::color(cx, "wave-error"),
            MarkKind::WarningProblem => theme::color(cx, "wave-warning"),
            MarkKind::WeakProblem => theme::color(cx, "fg-weak-warning"),
            MarkKind::BreakpointLine | MarkKind::UnverifiedBreakpoint => theme::color(cx, "bg-breakpoint-line"),
            MarkKind::InactiveBreakpoint => theme::color(cx, "fg-gutter"),
            MarkKind::ExecutionLine => theme::color(cx, "bg-debug-line"),
        };
        #[cfg(test)]
        let mut painted_marks = Vec::new();
        window.with_content_mask(Some(gpui_kit::ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, theme::color(cx, "bg-editor")));
            let rope = self.editor.read(cx).rope.clone();
            let heads: Vec<(u32, u32)> =
                selections.iter().map(|range| (buffer::line_col(&rope, range.head()).0, range.head())).collect();
            for (index, (text, shaped)) in layout.lines.iter().enumerate() {
                let line = layout.line_of[index];
                let row = layout.first_line + index as u32;
                let top = bounds.top() + line_height * row as f32 - scroll_top;
                let range = buffer::line_range(&rope, line);
                let text_origin = point(bounds.left() + text_left, top);
                let x_of = |offset: u32| {
                    shaped.x_for_index(buffer::byte_of_col(text, offset.clamp(range.start, range.end) - range.start))
                };
                // The area of `from..to` on this line; running past the line end covers the break.
                let area = |from: u32, to: u32, height: Pixels| {
                    let left = x_of(from);
                    let right = if to > range.end { x_of(range.end) + px(6.0) } else { x_of(to) };
                    Bounds::new(point(text_origin.x + left, top), size((right - left).max(px(0.0)), height))
                };
                let touches = |from: u32, to: u32| from <= range.end && to >= range.start && from < to;
                let on_line = |head: &(u32, u32)| head.0 == line;
                let caret_here = selections.iter().zip(&heads).any(|(range, head)| range.is_caret() && on_line(head));
                if focused && caret_here {
                    let row = Bounds::new(
                        point(bounds.left() + *gutter, top),
                        size(bounds.size.width - *gutter, line_height),
                    );
                    window.paint_quad(fill(row, theme::color(cx, "bg-current-line")));
                }
                // The breakpoint row is tinted, and the execution line goes over it.
                for kind in [MarkKind::BreakpointLine, MarkKind::UnverifiedBreakpoint, MarkKind::ExecutionLine] {
                    if marks.iter().any(|mark| mark.kind == kind && touches(mark.range.start, mark.range.end)) {
                        let row = Bounds::new(
                            point(bounds.left() + *gutter, top),
                            size(bounds.size.width - *gutter, line_height),
                        );
                        window.paint_quad(fill(row, mark_color(cx, kind)));
                        #[cfg(test)]
                        painted_marks.push((kind, row));
                    }
                }
                let backgrounds = |below: bool| {
                    marks.iter().filter(move |mark| {
                        mark.kind != MarkKind::ExcludedOccurrence
                            && !mark.kind.is_underline()
                            && !mark.kind.is_line()
                            && !mark.kind.is_breakpoint()
                            && mark.kind.is_below_selection() == below
                    })
                };
                for mark in backgrounds(true).filter(|mark| touches(mark.range.start, mark.range.end)) {
                    window.paint_quad(fill(
                        area(mark.range.start, mark.range.end, line_height),
                        mark_color(cx, mark.kind),
                    ));
                }
                for selected in selections.iter().filter(|range| touches(range.start(), range.end())) {
                    let selected = area(selected.start(), selected.end(), line_height);
                    window.paint_quad(fill(selected, theme::color(cx, "bg-selected")));
                }
                for mark in backgrounds(false).filter(|mark| touches(mark.range.start, mark.range.end)) {
                    let color = mark_color(cx, mark.kind);
                    let area = area(mark.range.start, mark.range.end, line_height);
                    if mark.kind == MarkKind::CurrentFindOccurrence {
                        let border = theme::color(cx, "fg-strong").blend(color.opacity(0.6));
                        window.paint_quad(quad(area, px(2.0), color, px(1.0), border, gpui_kit::BorderStyle::Solid));
                    } else {
                        window.paint_quad(quad(area, px(2.0), color, px(0.0), color, gpui_kit::BorderStyle::Solid));
                    }
                }
                if let Some(marked) = marked.as_ref().filter(|m| m.start <= range.end && m.end >= range.start) {
                    let (from, to) = (x_of(marked.start), x_of(marked.end));
                    let underline = Bounds::new(
                        point(text_origin.x + from, top + line_height - px(2.0)),
                        size((to - from).max(px(0.0)), px(1.0)),
                    );
                    window.paint_quad(fill(underline, theme::color(cx, "fg-editor")));
                }
                let _ = shaped.paint(text_origin, line_height, TextAlign::Left, None, window, cx);
                if let Some((placeholder, tail)) = placeholders.get(index).and_then(Option::as_ref) {
                    let origin = point(text_origin.x + shaped.width + px(4.0), top);
                    let chip = Bounds::new(origin, size(placeholder.width + px(8.0), line_height));
                    let background = theme::color(cx, "bg-fold-placeholder");
                    let solid = gpui_kit::BorderStyle::Solid;
                    window.paint_quad(quad(chip, px(3.0), background, px(0.0), background, solid));
                    let at = point(origin.x + px(4.0), top);
                    let _ = placeholder.paint(at, line_height, TextAlign::Left, None, window, cx);
                    let after = point(origin.x + chip.size.width + px(2.0), top);
                    let _ = tail.paint(after, line_height, TextAlign::Left, None, window, cx);
                }
                if let Some(marker) = markers.get(index).and_then(Option::as_ref).filter(|_| fold_column > px(0.0)) {
                    let x = bounds.left() + *gutter - (fold_column + marker.width) / 2.0;
                    let _ = marker.paint(point(x, top), line_height, TextAlign::Left, None, window, cx);
                }
                for mark in marks.iter().filter(|mark| mark.kind.is_underline()) {
                    if touches(mark.range.start, mark.range.end) {
                        let under = area(mark.range.start, mark.range.end.min(range.end), line_height);
                        let style = gpui_kit::UnderlineStyle {
                            thickness: px(1.0),
                            color: Some(mark_color(cx, mark.kind)),
                            wavy: true,
                        };
                        let origin = point(under.origin.x, top + line_height - px(3.0));
                        let width = under.size.width.max(px(4.0));
                        window.paint_underline(origin, width, &style);
                        #[cfg(test)]
                        painted_marks.push((mark.kind, Bounds::new(origin, size(width, px(1.0)))));
                    }
                }
                for mark in marks.iter().filter(|mark| mark.kind == MarkKind::ExcludedOccurrence) {
                    if touches(mark.range.start, mark.range.end) {
                        let struck = area(mark.range.start, mark.range.end.min(range.end), px(1.0));
                        let struck = Bounds { origin: point(struck.origin.x, top + line_height / 2.0), ..struck };
                        window.paint_quad(fill(struck, mark_color(cx, mark.kind)));
                    }
                }
                if let Some(number) = numbers.get(index).filter(|_| *gutter > px(0.0)) {
                    let x = bounds.left() + *gutter - fold_column - number.width - px(12.0);
                    let _ =
                        number.paint(point(x.max(bounds.left()), top), line_height, TextAlign::Left, None, window, cx);
                }
                let breakpoint =
                    marks.iter().find(|mark| mark.kind.is_breakpoint() && touches(mark.range.start, mark.range.end));
                if let Some(mark) = breakpoint.filter(|_| *gutter > px(0.0)) {
                    let diameter = px(10.0);
                    let dot = Bounds::new(
                        point(bounds.left() + px(4.0), top + (line_height - diameter) / 2.0),
                        size(diameter, diameter),
                    );
                    let solid = gpui_kit::BorderStyle::Solid;
                    #[cfg(test)]
                    painted_marks.push((mark.kind, dot));
                    if mark.kind == MarkKind::BreakpointLine {
                        let danger = theme::color(cx, "danger");
                        window.paint_quad(quad(dot, diameter / 2.0, danger, px(0.0), danger, solid));
                    } else if mark.kind == MarkKind::UnverifiedBreakpoint {
                        let grey = theme::color(cx, "fg-gutter");
                        window.paint_quad(quad(dot, diameter / 2.0, grey, px(0.0), grey, solid));
                    } else {
                        let ring = mark_color(cx, mark.kind);
                        window.paint_quad(quad(dot, diameter / 2.0, ring.opacity(0.0), px(1.5), ring, solid));
                    }
                }
                if focused {
                    for &(_, head) in heads.iter().filter(|head| on_line(head)) {
                        let caret_bounds =
                            Bounds::new(point(text_origin.x + x_of(head), top), size(px(2.0), line_height));
                        window.paint_quad(fill(caret_bounds, theme::color(cx, "caret")));
                    }
                }
            }
        });
        #[cfg(test)]
        {
            layout.painted_marks = painted_marks;
        }
        let layout = std::mem::replace(
            layout,
            Layout {
                bounds,
                gutter: *gutter,
                fold_column,
                text_left,
                line_height,
                first_line: 0,
                lines: Vec::new(),
                line_of: Vec::new(),
                #[cfg(test)]
                painted_marks: Vec::new(),
                #[cfg(test)]
                painted_colors: Vec::new(),
            },
        );
        self.editor.update(cx, |editor, _| editor.layout = Some(layout));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AppDirs;
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext};
    use ide_core::{Pos, TextEdit};
    use std::time::Duration;

    mod proofs8_caret_commands;
    mod proofs_editor_highlight_backend;
    mod proofs_editor_highlight_cmake;
    mod proofs_editor_highlight_interpolation;
    mod proofs_editor_highlight_support;

    /// A project in a temporary folder with `files`, and an editor on the first of them.
    struct Fixture {
        editor: Entity<Editor>,
        host: Arc<Host>,
        path: PathBuf,
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
    }

    impl Fixture {
        fn documents(&self) -> Arc<ide_workspace::Documents> {
            self.host.project("w1").expect("the project is open").documents.clone()
        }

        fn backend_text(&self) -> String {
            self.documents().text_of(&self.path).expect("the document is open")
        }
    }

    async fn open<'a>(cx: &'a mut TestAppContext, files: &[(&str, &str)]) -> (Fixture, &'a mut VisualTestContext) {
        cx.executor().allow_parking();
        let home = tempfile::tempdir().expect("a temporary home");
        let project = tempfile::tempdir().expect("a temporary project");
        let root = project.path().canonicalize().expect("the project folder exists");
        for (name, text) in files {
            std::fs::write(root.join(name), text).expect("the fixture file is written");
        }
        cx.update(|cx| {
            Backend::init(AppDirs::under(home.path()), cx).expect("the backend starts");
            cx.set_global(theme::ActiveTheme::default());
        });
        let path = root.join(files[0].0);
        let (root_for_open, path_for_open) = (root.clone(), path.clone());
        let opened = cx.update(|cx| {
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &root_for_open).await?;
                ide_host::ops::doc::open(&host, "w1", path_for_open).await
            })
        });
        let info = opened.await.expect("the task completes").expect("the document opens");
        let host = cx.update(|cx| cx.global::<Backend>().host.clone());
        let editor_host = host.clone();
        let (editor, cx) =
            cx.add_window_view(move |_, cx| Editor::new(editor_host, "w1", &info, cx).expect("the editor opens"));
        cx.update(|window, cx| {
            let focus = editor.read(cx).focus.clone();
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        (Fixture { editor, host, path, _home: home, _project: project }, cx)
    }

    /// Waits for work on the backend's own threads to reach the UI.
    fn settle(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..200 {
            // Timers run on the test clock; the backend's threads run on the real one.
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the backend did not deliver within two seconds");
    }

    #[gpui_kit::test]
    async fn typing_edits_the_backends_document(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("notes.txt", "world\n")]).await;
        let before = f.documents().version(f.editor.read_with(cx, |e, _| e.doc())).expect("a version");
        cx.simulate_input("hello ");
        f.editor.read_with(cx, |editor, _| {
            assert_eq!(editor.text(), "hello world\n");
            assert_eq!(editor.caret(), 6);
            assert!(editor.version() > before);
        });
        // The text lives in the backend: it is what a save, a search or a language server sees.
        assert_eq!(f.backend_text(), "hello world\n");
        f.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn keys_move_the_caret_and_edit(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("notes.txt", "ab\ncdef\n")]).await;
        cx.simulate_keystrokes("right right right");
        assert_eq!(f.editor.read_with(cx, |e, _| e.caret_position()), (1, 0));
        cx.simulate_keystrokes("end");
        assert_eq!(f.editor.read_with(cx, |e, _| e.caret_position()), (1, 4));
        // Up keeps the column where the line is long enough and clamps where it is not.
        cx.simulate_keystrokes("up");
        assert_eq!(f.editor.read_with(cx, |e, _| e.caret_position()), (0, 2));
        cx.simulate_keystrokes("down");
        assert_eq!(f.editor.read_with(cx, |e, _| e.caret_position()), (1, 4));
        cx.simulate_keystrokes("backspace enter");
        assert_eq!(f.backend_text(), "ab\ncde\n\n");
        cx.simulate_keystrokes("shift-up shift-up");
        assert_eq!(f.editor.read_with(cx, |e, _| e.selection()), 0..7);
        cx.simulate_keystrokes("delete");
        assert_eq!(f.backend_text(), "\n");
        f.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn an_input_method_composes_in_place(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("notes.txt", "x")]).await;
        f.editor.update_in(cx, |editor, window, cx| {
            editor.select(1, 1, cx);
            editor.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            editor.replace_and_mark_text_in_range(None, "ni", Some(2..2), window, cx);
            // The composition is in the document, marked, with the caret inside it.
            assert_eq!(editor.text(), "xni");
            assert_eq!(editor.marked_text_range(window, cx), Some(1..3));
            assert_eq!(editor.caret(), 3);
            // The platform asks where the composition is drawn, in UTF-16.
            assert_eq!(editor.text_for_range(1..3, &mut None, window, cx).as_deref(), Some("ni"));
            editor.replace_text_in_range(None, "你", window, cx);
            assert_eq!(editor.text(), "x你");
            assert_eq!(editor.marked_text_range(window, cx), None);
            assert_eq!(editor.caret(), 2);
            // Cancelling a composition leaves nothing behind.
            editor.replace_and_mark_text_in_range(None, "h", Some(1..1), window, cx);
            editor.replace_and_mark_text_in_range(None, "", None, window, cx);
            assert_eq!((editor.text().as_str(), editor.marked_text_range(window, cx)), ("x你", None));
        });
        assert_eq!(f.backend_text(), "x你");
        f.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn columns_are_utf16_and_map_to_pixels_and_back(cx: &mut TestAppContext) {
        // a (1 unit) 😀 (2 units) 中 (1 unit) b (1 unit)
        let (f, cx) = open(cx, &[("notes.txt", "a😀中b\nsecond\n")]).await;
        let positions: Vec<Point<Pixels>> = f
            .editor
            .read_with(cx, |e, _| [0, 1, 3, 4, 5].map(|o| e.position_of(o).expect("the line is painted")).to_vec());
        assert!(positions.windows(2).all(|w| w[0].x < w[1].x && w[0].y == w[1].y), "{positions:?}");
        f.editor.read_with(cx, |editor, _| {
            for (offset, position) in [0, 1, 3, 4, 5].into_iter().zip(&positions) {
                assert_eq!(editor.offset_at(*position + point(px(0.0), px(3.0))), Some(offset));
            }
            // Inside the surrogate pair is not a position: it rounds to the emoji's start.
            assert_eq!(editor.position_of(2), editor.position_of(1));
            let second = editor.position_of(6).expect("the second line is painted");
            assert!(second.y > positions[0].y && second.x == positions[0].x);
        });
        // A click places the caret at the nearest character boundary.
        cx.simulate_click(positions[3] + point(px(0.0), px(3.0)), Modifiers::default());
        assert_eq!(f.editor.read_with(cx, |e, _| (e.caret(), e.caret_position())), (4, (0, 4)));
        // The platform's text input asks for the caret rectangle in the same coordinates.
        let caret = f
            .editor
            .update_in(cx, |e, window, cx| e.bounds_for_range(4..4, Bounds::default(), window, cx))
            .expect("a caret rectangle");
        assert_eq!(caret.origin, positions[3]);
        f.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn a_long_file_costs_a_viewport_per_frame(cx: &mut TestAppContext) {
        let text: String = (1..=100_000).map(|n| format!("line {n}\n")).collect();
        let (f, cx) = open(cx, &[("long.txt", &text)]).await;
        let (painted, viewport_lines) = f.editor.read_with(cx, |e, cx| {
            let layout = e.layout.as_ref().expect("a frame was painted");
            (e.painted_lines().expect("painted"), (layout.bounds.size.height / e.line_height(cx)).ceil() as u32)
        });
        assert_eq!(painted.start, 0);
        assert!(painted.len() as u32 <= viewport_lines + 1, "{painted:?} for a viewport of {viewport_lines} lines");
        assert_eq!(f.editor.read_with(cx, |e, _| e.line_count()), 100_001);
        // Scrolling to the end paints the end, still one viewport's worth.
        f.editor.update(cx, |e, cx| e.scroll_by(px(1e9), cx));
        cx.run_until_parked();
        let painted = f.editor.read_with(cx, |e, _| e.painted_lines().expect("painted"));
        assert_eq!(painted.end, 100_001);
        assert!(painted.len() as u32 <= viewport_lines + 1);
        // Typing at the end of a long file is one small edit, not a copy of the document.
        f.editor.update(cx, |e, cx| e.select(u32::MAX, u32::MAX, cx));
        cx.simulate_input("end");
        assert!(f.backend_text().ends_with("line 100000\nend"));
        f.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn highlights_come_from_the_backends_grammar(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("build.zig", "const answer = 42; // why\n")]).await;
        settle(cx, |cx| !f.editor.read_with(cx, |e, _| e.highlights().is_empty()));
        f.editor.read_with(cx, |editor, _| {
            let kind_at =
                |offset: u32| editor.highlights().iter().find(|s| s.from <= offset && offset < s.to).map(|s| s.kind);
            assert_eq!(kind_at(0), Some(SyntaxHighlightKind::Keyword), "{:?}", editor.highlights());
            assert_eq!(kind_at(15), Some(SyntaxHighlightKind::Number));
            assert_eq!(kind_at(20), Some(SyntaxHighlightKind::Comment));
        });
        assert_eq!(syntax_token(SyntaxHighlightKind::Namespace), "syn-type");
        f.host.close_project("w1");
    }

    /// Languages the webview highlights with CodeMirror are highlighted by their grammar's
    /// own query here (ide-language's `native-highlights` feature).
    #[gpui_kit::test]
    async fn rust_is_highlighted_without_codemirror(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("main.rs", "fn main() {\n    let greeting = \"hi\"; // say it\n}\n")]).await;
        settle(cx, |cx| !f.editor.read_with(cx, |e, _| e.highlights().is_empty()));
        f.editor.read_with(cx, |editor, _| {
            let text = editor.text();
            let kind_of = |token: &str| {
                let at = text.find(token).expect("the token is in the text") as u32;
                editor.highlights().iter().find(|s| s.from <= at && at < s.to).map(|s| s.kind)
            };
            assert_eq!(kind_of("fn"), Some(SyntaxHighlightKind::Keyword));
            assert_eq!(kind_of("main"), Some(SyntaxHighlightKind::Function));
            assert_eq!(kind_of("\"hi\""), Some(SyntaxHighlightKind::String));
            assert_eq!(kind_of("// say it"), Some(SyntaxHighlightKind::Comment));
        });
        f.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn the_backends_own_edits_move_the_caret(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("notes.txt", "body\n")]).await;
        f.editor.update(cx, |e, cx| e.select(2, 2, cx));
        let doc = f.editor.read_with(cx, |e, _| e.doc());
        let start = Pos { line: 0, col: 0 };
        // A refactoring, a formatter or a reload from disk edits the document in the backend.
        f.documents()
            .apply_edits(
                doc,
                &[TextEdit { range: ide_core::Range { start, end: start }, new_text: "// header\n".to_owned() }],
            )
            .expect("the edit applies");
        settle(cx, |cx| f.editor.read_with(cx, |e, _| e.text().starts_with("// header")));
        f.editor.read_with(cx, |editor, _| {
            assert_eq!(editor.text(), "// header\nbody\n");
            assert_eq!(editor.caret_position(), (1, 2));
        });
        f.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn typing_survives_a_backend_edit_that_has_not_arrived_yet(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("notes.txt", "body\n")]).await;
        f.editor.update(cx, |e, cx| e.select(4, 4, cx));
        let doc = f.editor.read_with(cx, |e, _| e.doc());
        let start = Pos { line: 0, col: 0 };
        f.documents()
            .apply_edits(doc, &[TextEdit { range: ide_core::Range { start, end: start }, new_text: ">> ".to_owned() }])
            .expect("the edit applies");
        // The editor still shows the old version: its edit is rejected as out of sync, the text
        // is refreshed and the typed character goes in at the caret's new place.
        cx.simulate_input("!");
        assert_eq!(f.backend_text(), ">> body!\n");
        assert_eq!(f.editor.read_with(cx, |e, _| (e.text(), e.caret())), (">> body!\n".to_owned(), 8));
        f.host.close_project("w1");
    }

    #[test]
    fn finds_the_difference_between_two_texts() {
        let diff = |old: &str, new: &str| difference(&Rope::from_str(old), &Rope::from_str(new));
        assert_eq!(diff("same", "same"), []);
        assert_eq!(diff("body", ">> body"), [TextChange { from: 0, to: 0, insert: ">> ".to_owned() }]);
        assert_eq!(diff("a😀c", "a😀bc"), [TextChange { from: 3, to: 3, insert: "b".to_owned() }]);
        assert_eq!(diff("hello world", "hello"), [TextChange { from: 5, to: 11, insert: String::new() }]);
        assert_eq!(diff("aaa", "aa"), [TextChange { from: 2, to: 3, insert: String::new() }]);
        assert_eq!(diff("", "x"), [TextChange { from: 0, to: 0, insert: "x".to_owned() }]);
    }
}

#[cfg(test)]
mod editing_tests {
    use super::*;
    use crate::backend::AppDirs;
    use crate::editor_commands::change;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use ide_core::{Pos, TextEdit};
    use std::time::Duration;

    mod proofs_editor_clipboard_history;
    mod proofs_editor_multi_caret;

    struct Opened {
        editor: Entity<Editor>,
        host: Arc<Host>,
        path: PathBuf,
        _folders: (tempfile::TempDir, tempfile::TempDir),
    }

    async fn open<'a>(cx: &'a mut TestAppContext, text: &str) -> (Opened, &'a mut VisualTestContext) {
        cx.executor().allow_parking();
        let (home, project) = (tempfile::tempdir().expect("a home"), tempfile::tempdir().expect("a project"));
        let root = project.path().canonicalize().expect("the project folder exists");
        let path = root.join("notes.txt");
        std::fs::write(&path, text).expect("the file is written");
        cx.update(|cx| {
            Backend::init(AppDirs::under(home.path()), cx).expect("the backend starts");
            cx.set_global(theme::ActiveTheme::default());
        });
        let (open_root, open_path) = (root.clone(), path.clone());
        let opened = cx.update(|cx| {
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &open_root).await?;
                ide_host::ops::doc::open(&host, "w1", open_path).await
            })
        });
        let info = opened.await.expect("the task completes").expect("the document opens");
        let host = cx.update(|cx| cx.global::<Backend>().host.clone());
        let editor_host = host.clone();
        let (editor, cx) =
            cx.add_window_view(move |_, cx| Editor::new(editor_host, "w1", &info, cx).expect("the editor opens"));
        cx.update(|window, cx| {
            let focus = editor.read(cx).focus.clone();
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        (Opened { editor, host, path, _folders: (home, project) }, cx)
    }

    fn settle(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the backend did not deliver within three seconds");
    }

    fn text(o: &Opened, cx: &mut VisualTestContext) -> String {
        o.editor.read_with(cx, |e, _| e.text())
    }

    /// The owner hears what changed the text (typing with what was typed, a deletion, a completion,
    /// undo, a change of the backend), that the carets moved, and where the pointer rests over the
    /// text; a pointer handler takes a press before the caret moves; the overlay is drawn.
    #[gpui_kit::test]
    async fn the_owner_hears_edits_carets_and_the_pointer_and_may_take_a_press(cx: &mut TestAppContext) {
        use std::cell::{Cell, RefCell};
        let (o, cx) = open(cx, "alpha beta\n").await;
        let events: Rc<RefCell<Vec<EditorEvent>>> = Rc::default();
        let seen = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&o.editor, move |_, event: &EditorEvent, _| seen.borrow_mut().push(event.clone())).detach()
        });
        let take = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            std::mem::take(&mut *events.borrow_mut())
        };
        let edited = |origin: EditOrigin, from: u32, to: u32, insert: &str| EditorEvent::Edited {
            origin,
            changes: vec![TextChange { from, to, insert: insert.to_owned() }],
        };

        cx.simulate_input("x");
        assert_eq!(take(cx), [edited(EditOrigin::Typed("x".into()), 0, 0, "x")]);
        cx.simulate_keystrokes("backspace");
        assert_eq!(take(cx), [edited(EditOrigin::Deleted, 0, 1, "")]);
        cx.simulate_keystrokes("right");
        assert_eq!(take(cx), [EditorEvent::SelectionMoved]);
        // A caret that does not move tells nothing.
        o.editor.update(cx, |editor, cx| editor.select(1, 1, cx));
        assert_eq!(take(cx), []);
        let applied = o.editor.update(cx, |editor, cx| {
            editor.apply_edit_from(EditOrigin::Completion, &[change(5, 5, "Z")], vec![(6, 6)], 0, cx)
        });
        assert!(applied);
        assert_eq!(take(cx), [edited(EditOrigin::Completion, 5, 5, "Z")]);
        o.editor.update(cx, |editor, cx| editor.undo(cx));
        assert_eq!(take(cx), [edited(EditOrigin::History, 5, 6, "")]);
        cx.simulate_keystrokes("enter");
        assert!(
            matches!(take(cx).as_slice(), [EditorEvent::Edited { origin: EditOrigin::Typed(text), .. }] if text == "\n")
        );
        let host = o.host.clone();
        let doc = o.editor.read_with(cx, |editor, _| editor.doc());
        let edit = TextEdit {
            range: ide_core::Range::new(Pos { line: 0, col: 0 }, Pos { line: 0, col: 0 }),
            new_text: "// ".into(),
        };
        host.project("w1").expect("the project").documents.apply_edits(doc, &[edit]).expect("the backend edits");
        settle(cx, |cx| text(&o, cx).starts_with("// "));
        assert!(matches!(take(cx).as_slice(), [EditorEvent::Edited { origin: EditOrigin::External, .. }]));

        // The pointer over the text, then beside it.
        let (at, line_height) =
            o.editor.read_with(cx, |editor, cx| (editor.position_of(7).expect("painted"), editor.line_height(cx)));
        assert_eq!(line_height, cx.update(|_, cx| theme::length(cx, "lh-code")));
        let inside = point(at.x + px(1.0), at.y + line_height / 2.0);
        cx.simulate_mouse_move(inside, None, Modifiers::default());
        assert_eq!(take(cx), [EditorEvent::Hovered { offset: Some(7), modifiers: Modifiers::default() }]);
        cx.simulate_mouse_move(point(inside.x + px(2000.0), inside.y), None, Modifiers::default());
        assert_eq!(take(cx), [EditorEvent::Hovered { offset: None, modifiers: Modifiers::default() }]);

        // A handler that takes the press: the caret stays.
        let taken: Rc<Cell<Option<Option<u32>>>> = Rc::default();
        let took = taken.clone();
        let handler: PointerHandler = Rc::new(move |_, offset, _, _| {
            took.set(Some(offset));
            true
        });
        o.editor.update(cx, |editor, _| editor.set_pointer_handler(Some(handler)));
        let caret = o.editor.read_with(cx, |editor, _| editor.caret());
        cx.simulate_click(inside, Modifiers::default());
        assert_eq!(taken.get(), Some(Some(7)));
        assert_eq!(o.editor.read_with(cx, |editor, _| editor.caret()), caret);
        o.editor.update(cx, |editor, _| editor.set_pointer_handler(None));
        cx.simulate_click(inside, Modifiers::default());
        assert_eq!(o.editor.read_with(cx, |editor, _| editor.caret()), 7);

        // The overlay is drawn with the editor.
        struct Marker;
        impl Render for Marker {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().debug_selector(|| "editor-overlay-marker".to_owned()).size(px(4.0))
            }
        }
        let marker = cx.update(|_, cx| cx.new(|_| Marker));
        o.editor.update(cx, |editor, cx| editor.set_overlay(Some(marker.into()), cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("editor-overlay-marker").is_some());
        assert!(o.editor.read_with(cx, |editor, _| editor.overlay().is_some()));
    }

    #[gpui_kit::test]
    async fn undo_and_redo_walk_groups_of_typing(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "end\n").await;
        cx.simulate_input("one ");
        // Moving the caret ends the group; typing after it is a second one.
        cx.simulate_keystrokes("right left");
        cx.simulate_input("two ");
        assert_eq!(text(&o, cx), "one two end\n");
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&o, cx), "one end\n");
        assert_eq!(o.editor.read_with(cx, |e, _| e.caret()), 4);
        cx.simulate_keystrokes("cmd-z");
        assert_eq!((text(&o, cx).as_str(), o.editor.read_with(cx, |e, _| e.caret())), ("end\n", 0));
        // Nothing left to undo: the key does nothing.
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&o, cx), "end\n");
        cx.simulate_keystrokes("cmd-shift-z cmd-shift-z");
        assert_eq!((text(&o, cx).as_str(), o.editor.read_with(cx, |e, _| e.caret())), ("one two end\n", 8));
        // The backend holds the same text after every step.
        assert_eq!(o.host.project("w1").unwrap().documents.text_of(&o.path).as_deref(), Some("one two end\n"));
        // A new edit after an undo drops what could be redone.
        cx.simulate_keystrokes("cmd-z");
        cx.simulate_input("x");
        assert!(!o.editor.read_with(cx, |e, _| e.can_redo()));
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn a_backend_edit_is_undone_like_any_other(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "body\n").await;
        let doc = o.editor.read_with(cx, |e, _| e.doc());
        let start = Pos { line: 0, col: 0 };
        let edit = TextEdit { range: ide_core::Range { start, end: start }, new_text: "// formatted\n".to_owned() };
        o.host.project("w1").unwrap().documents.apply_edits(doc, &[edit]).expect("the edit applies");
        settle(cx, |cx| o.editor.read_with(cx, |e, _| e.text().starts_with("// formatted")));
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&o, cx), "body\n");
        cx.simulate_keystrokes("cmd-shift-z");
        assert_eq!(text(&o, cx), "// formatted\nbody\n");
        o.host.close_project("w1");
    }

    // src/editor/core/cm/caretCommands.test.ts, baseKeymap.ts (word, line and document movement)
    #[gpui_kit::test]
    async fn option_and_command_move_by_word_line_and_document(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "alpha beta gamma\nsecond line\n").await;
        let caret = |cx: &mut VisualTestContext| o.editor.read_with(cx, |e, _| e.caret());
        cx.simulate_keystrokes("alt-right alt-right");
        assert_eq!(caret(cx), 10);
        cx.simulate_keystrokes("alt-left");
        assert_eq!(caret(cx), 6);
        cx.simulate_keystrokes("cmd-right");
        assert_eq!(caret(cx), 16);
        cx.simulate_keystrokes("shift-cmd-left");
        assert_eq!(o.editor.read_with(cx, |e, _| e.selection()), 0..16);
        cx.simulate_keystrokes("cmd-down");
        assert_eq!(caret(cx), 29);
        cx.simulate_keystrokes("cmd-up alt-right alt-right");
        // ⌥⌫ deletes the word before the caret, ⌥⌦ the one after, ⌘⌫ to the line's start.
        cx.simulate_keystrokes("alt-backspace");
        assert_eq!(text(&o, cx), "alpha  gamma\nsecond line\n");
        cx.simulate_keystrokes("alt-delete");
        assert_eq!(text(&o, cx), "alpha \nsecond line\n");
        cx.simulate_keystrokes("cmd-backspace");
        assert_eq!(text(&o, cx), "\nsecond line\n");
        // Each is one undo step.
        cx.simulate_keystrokes("cmd-z cmd-z cmd-z");
        assert_eq!(text(&o, cx), "alpha beta gamma\nsecond line\n");
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn the_mouse_selects_by_drag_word_and_line(cx: &mut TestAppContext) {
        use gpui_kit::{Modifiers, MouseDownEvent, MouseUpEvent};
        let (o, cx) = open(cx, "alpha beta\nsecond\n").await;
        let at = |cx: &mut VisualTestContext, offset: u32| {
            o.editor.read_with(cx, |e, _| e.position_of(offset).expect("painted")) + point(px(0.0), px(3.0))
        };
        // Press at 2, drag to 8, release.
        let (from, to) = (at(cx, 2), at(cx, 8));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
        assert_eq!(o.editor.read_with(cx, |e, _| (e.selection(), e.caret())), (2..8, 8));
        // Moving without a button held does not select.
        let origin = at(cx, 0);
        cx.simulate_mouse_move(origin, None, Modifiers::default());
        assert_eq!(o.editor.read_with(cx, |e, _| e.selection()), 2..8);
        // A double click selects the word, a triple click the line with its break.
        let word = at(cx, 7);
        let click = |cx: &mut VisualTestContext, count: usize| {
            cx.simulate_event(MouseDownEvent {
                position: word,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                position: word,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: count,
            });
        };
        click(cx, 2);
        assert_eq!(o.editor.read_with(cx, |e, _| e.selected_text()), "beta");
        click(cx, 3);
        assert_eq!(o.editor.read_with(cx, |e, _| e.selected_text()), "alpha beta\n");
        // Shift+click extends from the anchor.
        let (first, second) = (at(cx, 1), at(cx, 5));
        cx.simulate_click(first, Modifiers::default());
        cx.simulate_click(second, Modifiers { shift: true, ..Modifiers::default() });
        assert_eq!(o.editor.read_with(cx, |e, _| e.selection()), 1..5);
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn clipboard_keys_copy_cut_and_paste(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "alpha beta\n").await;
        o.editor.update(cx, |e, cx| e.select(0, 6, cx));
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("alpha "));
        cx.simulate_keystrokes("end cmd-v");
        assert_eq!(text(&o, cx), "alpha betaalpha \n");
        o.editor.update(cx, |e, cx| e.select(0, 6, cx));
        cx.simulate_keystrokes("cmd-x");
        assert_eq!(text(&o, cx), "betaalpha \n");
        // Without a selection the caret line is copied, without its line break.
        cx.simulate_keystrokes("cmd-c");
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("betaalpha "));
        cx.simulate_keystrokes("cmd-a");
        assert_eq!(o.editor.read_with(cx, |e, _| e.selection()), 0..11);
        // A paste is one undo step, whatever its length.
        cx.simulate_keystrokes("cmd-v cmd-z");
        assert_eq!(text(&o, cx), "betaalpha \n");
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn saving_writes_the_file_and_clears_the_dirty_state(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "draft\n").await;
        assert!(!o.editor.read_with(cx, |e, _| e.is_dirty()));
        cx.simulate_input("final ");
        settle(cx, |cx| o.editor.read_with(cx, |e, _| e.is_dirty()));
        assert_eq!(std::fs::read_to_string(&o.path).expect("readable"), "draft\n");
        let saved = o.editor.read_with(cx, |e, cx| e.save(cx));
        saved.await.expect("the task completes").expect("the file saves");
        assert_eq!(std::fs::read_to_string(&o.path).expect("readable"), "final draft\n");
        settle(cx, |cx| !o.editor.read_with(cx, |e, _| e.is_dirty()));
        o.host.close_project("w1");
    }

    fn selections(o: &Opened, cx: &mut VisualTestContext) -> Vec<(u32, u32)> {
        o.editor.read_with(cx, |e, _| e.selections().to_vec())
    }

    #[gpui_kit::test]
    async fn several_carets_type_delete_and_move_together(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "one\ntwo\nthree\n").await;
        o.editor.update(cx, |e, cx| e.set_selections(vec![(8, 8), (0, 0), (4, 4)], 0, cx));
        // Sorted, with the main range still the one at 8.
        assert_eq!(selections(&o, cx), [(0, 0), (4, 4), (8, 8)]);
        assert_eq!(o.editor.read_with(cx, |e, _| (e.main_index(), e.caret())), (2, 8));
        cx.simulate_input("> ");
        assert_eq!(text(&o, cx), "> one\n> two\n> three\n");
        assert_eq!(selections(&o, cx), [(2, 2), (8, 8), (14, 14)]);
        cx.simulate_keystrokes("backspace end");
        assert_eq!(text(&o, cx), ">one\n>two\n>three\n");
        assert_eq!(selections(&o, cx), [(4, 4), (9, 9), (16, 16)]);
        cx.simulate_input("!");
        cx.simulate_keystrokes("shift-left");
        assert_eq!(selections(&o, cx), [(5, 4), (11, 10), (19, 18)]);
        cx.simulate_keystrokes("delete");
        assert_eq!(text(&o, cx), ">one\n>two\n>three\n");
        // Each step is one undo for every caret; undo brings the selections back.
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&o, cx), ">one!\n>two!\n>three!\n");
        assert_eq!(selections(&o, cx), [(5, 4), (11, 10), (19, 18)]);
        // Up and Down move every caret, each keeping its column; carets that meet merge.
        o.editor.update(cx, |e, cx| e.set_selections(vec![(4, 4), (10, 10)], 1, cx));
        cx.simulate_keystrokes("up");
        assert_eq!(selections(&o, cx), [(0, 0), (4, 4)]);
        cx.simulate_keystrokes("down");
        assert_eq!(selections(&o, cx), [(10, 10)]);
        o.editor.update(cx, |e, cx| e.set_selections(vec![(7, 7), (13, 13)], 1, cx));
        cx.simulate_keystrokes("shift-alt-right");
        assert_eq!(selections(&o, cx), [(7, 10), (13, 18)]);
        assert_eq!(o.editor.read_with(cx, |e, _| e.selected_text()), "three");
        cx.simulate_keystrokes("enter");
        assert_eq!(text(&o, cx), ">one!\n>\n!\n>\n!\n");
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn several_carets_paste_a_line_each_and_cut_together(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "a\nb\nc").await;
        o.editor.update(cx, |e, cx| e.set_selections(vec![(1, 1), (3, 3), (5, 5)], 0, cx));
        cx.write_to_clipboard(ClipboardItem::new_string("1\n2\n3".to_owned()));
        cx.simulate_keystrokes("cmd-v");
        assert_eq!(text(&o, cx), "a1\nb2\nc3");
        assert_eq!(selections(&o, cx), [(2, 2), (5, 5), (8, 8)]);
        // The same text at every caret when the line counts differ.
        cx.write_to_clipboard(ClipboardItem::new_string("x".to_owned()));
        cx.simulate_keystrokes("cmd-v");
        assert_eq!(text(&o, cx), "a1x\nb2x\nc3x");
        o.editor.update(cx, |e, cx| e.set_selections(vec![(0, 1), (4, 5)], 1, cx));
        cx.simulate_keystrokes("cmd-x");
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("a\nb"));
        assert_eq!(text(&o, cx), "1x\n2x\nc3x");
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&o, cx), "a1x\nb2x\nc3x");
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn an_input_method_composes_at_every_caret(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "a\nb").await;
        o.editor.update_in(cx, |editor, window, cx| {
            editor.set_selections(vec![(1, 1), (3, 3)], 1, cx);
            editor.replace_and_mark_text_in_range(None, "n", None, window, cx);
            editor.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            assert_eq!(editor.text(), "ani\nbni");
            assert_eq!(editor.marked_text_range(window, cx), Some(5..7));
            editor.replace_text_in_range(None, "你", window, cx);
            assert_eq!(editor.text(), "a你\nb你");
            assert_eq!(editor.selections(), [(2, 2), (5, 5)]);
        });
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn escape_keeps_the_main_range_then_collapses_it(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "alpha beta\n").await;
        o.editor.update(cx, |e, cx| e.set_selections(vec![(0, 0), (6, 10)], 1, cx));
        cx.simulate_keystrokes("escape");
        assert_eq!(selections(&o, cx), [(6, 10)]);
        cx.simulate_keystrokes("escape");
        assert_eq!(selections(&o, cx), [(10, 10)]);
        cx.simulate_keystrokes("escape");
        assert_eq!(selections(&o, cx), [(10, 10)]);
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn the_keymaps_caret_click_adds_and_removes_carets(cx: &mut TestAppContext) {
        use gpui_kit::Modifiers;
        let (o, cx) = open(cx, "alpha beta\nsecond\n").await;
        cx.update(|_, cx| crate::platform::input::init(cx));
        let at = |cx: &mut VisualTestContext, offset: u32| {
            o.editor.read_with(cx, |e, _| e.position_of(offset).expect("painted")) + point(px(0.0), px(3.0))
        };
        let gesture = Modifiers { alt: true, shift: true, ..Modifiers::default() };
        o.editor.update(cx, |e, cx| e.select(2, 2, cx));
        let (seven, thirteen) = (at(cx, 7), at(cx, 13));
        cx.simulate_click(seven, gesture);
        cx.simulate_click(thirteen, gesture);
        assert_eq!(selections(&o, cx), [(2, 2), (7, 7), (13, 13)]);
        assert_eq!(o.editor.read_with(cx, |e, _| e.caret()), 13);
        // The gesture on a caret removes it.
        cx.simulate_click(seven, gesture);
        assert_eq!(selections(&o, cx), [(2, 2), (13, 13)]);
        cx.simulate_input("#");
        assert_eq!(text(&o, cx), "al#pha beta\nse#cond\n");
        // A plain click goes back to one caret.
        cx.simulate_click(seven, Modifiers::default());
        assert_eq!(selections(&o, cx), [(7, 7)]);
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn marks_follow_edits_until_they_are_replaced(cx: &mut TestAppContext) {
        use super::super::marks::{Mark, MarkKind, MarkLayer};
        let (o, cx) = open(cx, "find me, find you\n").await;
        let marks = vec![Mark::new(9..13, MarkKind::FindOccurrence), Mark::new(0..4, MarkKind::CurrentFindOccurrence)];
        o.editor.update(cx, |e, cx| e.set_marks(MarkLayer::Find, marks, cx));
        let marks = |cx: &mut VisualTestContext| o.editor.read_with(cx, |e, _| e.marks(MarkLayer::Find).to_vec());
        assert_eq!(
            marks(cx),
            [Mark::new(0..4, MarkKind::CurrentFindOccurrence), Mark::new(9..13, MarkKind::FindOccurrence)]
        );
        cx.simulate_input(">> ");
        assert_eq!(
            marks(cx),
            [Mark::new(3..7, MarkKind::CurrentFindOccurrence), Mark::new(12..16, MarkKind::FindOccurrence)]
        );
        // A mark whose text goes goes with it.
        o.editor.update(cx, |e, cx| e.select(3, 8, cx));
        cx.simulate_keystrokes("delete");
        assert_eq!(marks(cx), [Mark::new(7..11, MarkKind::FindOccurrence)]);
        o.editor.update(cx, |e, cx| e.set_marks(MarkLayer::Find, Vec::new(), cx));
        assert_eq!(marks(cx), []);
        o.host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn an_edit_of_several_changes_is_one_undo_step_that_sibling_editors_follow(cx: &mut TestAppContext) {
        let (o, cx) = open(cx, "body\n").await;
        let documents = o.host.project("w1").expect("the project is open").documents.clone();
        let doc = o.editor.read_with(cx, |e, _| e.doc());
        let (host, path) = (o.host.clone(), o.path.clone());
        let opened = cx.update(|_, cx| {
            cx.global::<Backend>()
                .with_host(move |host| async move { ide_host::ops::doc::open(&host, "w1", path).await })
        });
        let info = opened.await.expect("the task completes").expect("the document opens again");
        let sibling = cx.new(|cx| Editor::new(host.clone(), "w1", &info, cx).expect("a second editor"));
        sibling.update(cx, |e, cx| e.select(2, 2, cx));
        o.editor.update(cx, |e, cx| e.select(4, 4, cx));
        let before = documents.version(doc).expect("a version");
        let changes = [change(0, 0, "<"), change(2, 2, "|"), change(4, 4, ">")];
        assert!(o.editor.update(cx, |e, cx| e.apply_edit(&changes, vec![(1, 1), (7, 7)], 1, cx)));
        assert_eq!(text(&o, cx), "<bo|dy>\n");
        assert_eq!(documents.version(doc).expect("a version"), before + 1);
        assert_eq!(selections(&o, cx), [(1, 1), (7, 7)]);
        // The other editor of the document follows each change: its caret stays between "bo" and "dy".
        assert_eq!(sibling.read_with(cx, |e, _| (e.text(), e.caret())), ("<bo|dy>\n".to_owned(), 4));
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&o, cx), "body\n");
        assert_eq!(selections(&o, cx), [(4, 4)]);
        assert_eq!(sibling.read_with(cx, |e, _| e.text()), "body\n");
        // A read-only editor refuses.
        let read_only = DocumentInfo { read_only: true, ..info };
        let viewer = cx.new(|cx| Editor::new(host.clone(), "w1", &read_only, cx).expect("a read-only editor"));
        assert!(!viewer.update(cx, |e, cx| e.apply_edit(&changes, vec![(0, 0)], 0, cx)));
        assert_eq!(text(&o, cx), "body\n");
        o.host.close_project("w1");
    }
}
