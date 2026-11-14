//! The Run/Debug configuration chooser popup (⌃⌥R, and the title bar widget's dropdown).
//!
//! It lists "Current File", the pinned and recent configurations and the rest by kind, as
//! [`build_rows`] orders them, filtered by what is typed. Rows 1–9 carry a number while the chooser
//! runs or debugs and nothing is typed. The selected row offers Pin and Run / Debug, or Rerun / Stop
//! while the configuration runs. Enter, a click or a number acts on a row; Shift switches Run and
//! Debug; F4 edits; Delete removes; Escape closes.
//!
//! [`ConfigChooser`] owns no process and changes nothing in the store: what the user chose is a
//! [`ConfigChooserEvent`] the workbench carries out (selecting, starting, pinning and deleting all
//! write the project's storage).

use crate::platform::i18n::Localization;
use crate::platform::input;
use crate::run::config_chooser::{ChooserState, Row, build_rows, mnemonics};
use crate::run::config_types::RunConfigTypes;
use crate::run::configs::config_icon;
use crate::run::sessions::selected_target;
use crate::run::store::{CURRENT_FILE, ChooserMode, RunStore};
use crate::theme;
use crate::ui::matching::index_runs;
use crate::ui::text_field::{TextField, TextFieldEvent};
use gpui_kit::{
    AnyElement, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Keystroke,
    ModifiersChangedEvent, MouseMoveEvent, Render, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle,
    Window, div, prelude::*, px, uniform_list,
};
use std::rc::Rc;

/// Height of a row.
const ROW: f32 = 24.0;
/// Rows shown before the list scrolls.
const VISIBLE_ROWS: usize = 14;
const WIDTH: f32 = 340.0;
/// How many rows Page Up and Page Down move by.
const PAGE_ROWS: isize = 10;

/// What choosing a configuration does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChooserAction {
    /// Select it in the title bar widget.
    Select,
    /// Select it and run it.
    Run,
    /// Select it and debug it.
    Debug,
    /// Select it and open Edit Configurations on it.
    Edit,
}

/// What the chooser asks of its window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigChooserEvent {
    /// Escape, or a click beside the popup.
    Dismissed,
    /// A configuration, or "Current File" ([`CURRENT_FILE`]), was chosen. Closes the chooser.
    Chosen { id: String, how: ChooserAction },
    /// "Edit Configurations…". Closes the chooser.
    EditConfigs,
    /// "Save '<name>' Configuration" of the selected temporary configuration. Closes the chooser.
    SaveTemporary(String),
    /// Rerun the live session with this id. Closes the chooser.
    Rerun(String),
    /// Stop the live session with this id; the chooser stays open.
    Stop(String),
    /// Pin the configuration with this id, or unpin it; the chooser stays open.
    TogglePinned(String),
    /// Delete the configuration with this id; the chooser stays open.
    Delete(String),
}

/// An inline button of the selected row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineButton {
    Pin,
    Run,
    Debug,
    Rerun,
    Stop,
}

impl InlineButton {
    fn selector(self) -> &'static str {
        match self {
            Self::Pin => "run-chooser-pin",
            Self::Run => "run-chooser-run",
            Self::Debug => "run-chooser-debug",
            Self::Rerun => "run-chooser-rerun",
            Self::Stop => "run-chooser-stop",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Run => "run",
            Self::Debug => "debug",
            Self::Rerun => "rerun-filled",
            Self::Stop => "stop-filled",
        }
    }
}

/// A row with what it shows besides its configuration.
#[derive(Clone, Debug)]
struct Entry {
    row: Row,
    /// The row as [`ConfigChooser::lines`] spells it.
    line: String,
    number: Option<u8>,
    /// The live session of the row's configuration that started last.
    running: Option<String>,
    temporary: bool,
    pinned: bool,
    selected: bool,
}

pub struct ConfigChooser {
    store: Entity<RunStore>,
    types: Rc<RunConfigTypes>,
    mode: ChooserMode,
    field: Entity<TextField>,
    entries: Rc<Vec<Entry>>,
    /// Index into `entries` of the selected row.
    active: Option<usize>,
    /// Shift is held: Run and Debug trade places.
    shift: bool,
    debug_available: bool,
    edit_shortcut: Option<String>,
    scroll: UniformListScrollHandle,
    key_claim: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConfigChooserEvent> for ConfigChooser {}

/// The other of Run and Debug.
fn alternate(mode: ChooserMode) -> ChooserAction {
    match mode {
        ChooserMode::Debug => ChooserAction::Run,
        ChooserMode::Run | ChooserMode::Select => ChooserAction::Debug,
    }
}

fn action_of(mode: ChooserMode) -> ChooserAction {
    match mode {
        ChooserMode::Select => ChooserAction::Select,
        ChooserMode::Run => ChooserAction::Run,
        ChooserMode::Debug => ChooserAction::Debug,
    }
}

fn tx(cx: &App, text: &str) -> String {
    cx.try_global::<Localization>().map_or_else(|| text.to_owned(), |localization| localization.0.tx(text))
}

/// Text built at run time, translated through the catalog's templates (`Run '{name}'`).
fn tx_template(cx: &App, text: String) -> String {
    cx.try_global::<Localization>().map_or_else(|| text.clone(), |localization| localization.0.tx_template(&text))
}

/// `text` with the characters at `indices` on the search-match colour.
fn marked_text(cx: &App, text: &str, indices: Option<&[usize]>) -> Div {
    let Some(indices) = indices.filter(|indices| !indices.is_empty()) else {
        return div().flex().min_w_0().overflow_hidden().child(SharedString::from(text.to_owned()));
    };
    let (match_bg, match_fg) = (theme::color(cx, "bg-search-match"), theme::color(cx, "fg-search-match"));
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut at = 0;
    for run in index_runs(text, indices) {
        if run.start < at {
            continue;
        }
        if run.start > at {
            parts.push((text[at..run.start].to_owned(), false));
        }
        at = run.end;
        parts.push((text[run].to_owned(), true));
    }
    if at < text.len() {
        parts.push((text[at..].to_owned(), false));
    }
    div().flex().min_w_0().overflow_hidden().children(parts.into_iter().map(|(part, matched)| {
        let part = div().child(SharedString::from(part));
        if matched { part.bg(match_bg).text_color(match_fg) } else { part }
    }))
}

impl ConfigChooser {
    /// The chooser over the configurations of `store`. `mode` says what choosing a row does: select
    /// it (the widget), run it (⌃⌥R) or debug it.
    pub fn new(store: Entity<RunStore>, types: Rc<RunConfigTypes>, mode: ChooserMode, cx: &mut Context<Self>) -> Self {
        let placeholder = tx(cx, "Search configurations");
        let field = cx.new(|cx| TextField::new(cx).with_placeholder(placeholder));
        let subscriptions = vec![
            cx.observe(&store, |chooser, _, cx| chooser.refresh(false, cx)),
            cx.subscribe(&field, |chooser, _, event: &TextFieldEvent, cx| match event {
                TextFieldEvent::Changed => chooser.refresh(true, cx),
                // Enter is the chooser's own key: it is claimed before the field sees it.
                TextFieldEvent::Submitted => {}
                TextFieldEvent::Cancelled => cx.emit(ConfigChooserEvent::Dismissed),
            }),
        ];
        let claimant = cx.weak_entity();
        let key_claim = input::claim_keys(
            field.focus_handle(cx),
            move |keystroke, _, cx| {
                claimant.update(cx, |chooser, cx| chooser.claim_key(keystroke, cx)).unwrap_or(false)
            },
            cx,
        );
        cx.on_release(|chooser, cx| input::release_keys(chooser.key_claim, cx)).detach();
        let mut chooser = Self {
            store,
            types,
            mode,
            field,
            entries: Rc::default(),
            active: None,
            shift: false,
            debug_available: true,
            edit_shortcut: None,
            scroll: UniformListScrollHandle::new(),
            key_claim,
            _subscriptions: subscriptions,
        };
        chooser.refresh(false, cx);
        chooser.active = chooser.initial();
        chooser
    }

    /// Whether Debug is offered, and the shortcut shown beside "Edit Configurations…".
    pub fn set_capabilities(&mut self, debug: bool, edit_shortcut: Option<String>, cx: &mut Context<Self>) {
        (self.debug_available, self.edit_shortcut) = (debug, edit_shortcut);
        cx.notify();
    }

    pub fn mode(&self) -> ChooserMode {
        self.mode
    }

    pub fn field(&self) -> &Entity<TextField> {
        &self.field
    }

    /// The rows, as the chooser's tests spell them: `[Section]`, a configuration's name, `current`,
    /// `save` and `edit`.
    pub fn lines(&self) -> Vec<String> {
        self.entries.iter().map(|entry| entry.line.clone()).collect()
    }

    /// The selected row, as [`Self::lines`] spells it.
    pub fn selected(&self) -> Option<String> {
        self.entries.get(self.active?).map(|entry| entry.line.clone())
    }

    /// The numbered rows: the configuration's name and its number.
    pub fn numbers(&self) -> Vec<(String, u8)> {
        self.entries.iter().filter_map(|entry| Some((entry.line.clone(), entry.number?))).collect()
    }

    /// The configurations marked as running.
    pub fn running(&self) -> Vec<String> {
        self.entries.iter().filter(|entry| entry.running.is_some()).map(|entry| entry.line.clone()).collect()
    }

    /// The inline buttons of the selected row: their titles and whether they can be used.
    pub fn buttons(&self, cx: &App) -> Vec<(String, bool)> {
        let Some(entry) = self.active.and_then(|index| self.entries.get(index)) else { return Vec::new() };
        self.inline_buttons(entry).into_iter().map(|button| (self.button_title(entry, button, cx), true)).collect()
    }

    /// "Run" or "Debug" over the list while the chooser runs or debugs (Shift flips it).
    pub fn caption(&self, cx: &App) -> Option<String> {
        let action = self.executor();
        (self.mode != ChooserMode::Select).then(|| tx(cx, if action == ChooserAction::Debug { "Debug" } else { "Run" }))
    }

    /// The hint under the list while the chooser runs or debugs.
    pub fn hint(&self, cx: &App) -> Option<String> {
        let text = match self.mode {
            ChooserMode::Select => return None,
            ChooserMode::Run => "Hold Shift to Debug, F4 to edit, Delete to remove",
            ChooserMode::Debug => "Hold Shift to Run, F4 to edit, Delete to remove",
        };
        Some(tx(cx, text))
    }

    /// What Enter does now: the mode's action, or the other one while Shift is held.
    fn executor(&self) -> ChooserAction {
        match self.mode {
            ChooserMode::Select => ChooserAction::Select,
            mode if self.shift => alternate(mode),
            mode => action_of(mode),
        }
    }

    /// The row of the selected configuration (or "Current File"), else the first that can be chosen.
    fn initial(&self) -> Option<usize> {
        let selected = self.entries.iter().position(|entry| entry.selected);
        selected.or_else(|| self.entries.iter().position(|entry| !matches!(entry.row, Row::Section { .. })))
    }

    /// Rebuilds the rows. A new query selects its first row; otherwise the selection stays on its row.
    fn refresh(&mut self, query_changed: bool, cx: &mut Context<Self>) {
        let kept = self.active.and_then(|index| self.entries.get(index)).map(|entry| entry.row.key());
        let query = self.field.read(cx).text().to_owned();
        let store = self.store.read(cx);
        let state = ChooserState {
            configs: store.configs(),
            selected: store.selected_config(),
            temporary: &store.temporary,
            pinned: &store.pinned,
            recent: &store.recent,
        };
        let rows = build_rows(state, &query, &self.types);
        let numbers = if self.mode == ChooserMode::Select || !query.is_empty() { Vec::new() } else { mnemonics(&rows) };
        let selected_id = store.selected_id.as_deref();
        let entries: Vec<Entry> = rows
            .into_iter()
            .map(|row| {
                let key = row.key();
                let number = numbers.iter().find(|(numbered, _)| *numbered == key).map(|(_, number)| *number);
                let (line, running, temporary, pinned, selected) = match &row {
                    Row::Section { label, .. } => (format!("[{label}]"), None, false, false, false),
                    Row::Current { .. } => {
                        ("current".to_owned(), None, false, false, selected_id == Some(CURRENT_FILE))
                    }
                    Row::Save { .. } => ("save".to_owned(), None, false, false, false),
                    Row::Edit => ("edit".to_owned(), None, false, false, false),
                    Row::Config { config, .. } => (
                        config.name.clone(),
                        selected_target(store, Some(&config.id)).map(|session| session.id.clone()),
                        store.temporary.contains(&config.id),
                        store.pinned.contains(&config.id),
                        selected_id == Some(config.id.as_str()),
                    ),
                };
                Entry { row, line, number, running, temporary, pinned, selected }
            })
            .collect();
        self.entries = Rc::new(entries);
        let selectable = |index: &usize| !matches!(self.entries[*index].row, Row::Section { .. });
        self.active = if query_changed {
            (0..self.entries.len()).find(selectable)
        } else {
            let found = kept.and_then(|key| self.entries.iter().position(|entry| entry.row.key() == key));
            found.or_else(|| {
                let last = self.entries.len().checked_sub(1)?;
                self.active.map(|index| index.min(last)).filter(selectable).or_else(|| self.initial())
            })
        };
        cx.notify();
    }

    /// Selects the row at `index` (a section header is skipped).
    pub fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.entries.get(index).is_some_and(|entry| !matches!(entry.row, Row::Section { .. })) {
            self.active = Some(index);
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    /// Moves the selection by `by` rows over the section headers, going round at either end when `wrap`.
    fn step(&mut self, by: isize, wrap: bool, cx: &mut Context<Self>) {
        let selectable: Vec<usize> =
            (0..self.entries.len()).filter(|&index| !matches!(self.entries[index].row, Row::Section { .. })).collect();
        let Some(last) = selectable.len().checked_sub(1) else { return };
        let at = self.active.and_then(|active| selectable.iter().position(|&index| index == active));
        let next = match at {
            None => 0,
            Some(0) if by < 0 && wrap => last,
            Some(at) if at == last && by > 0 && wrap => 0,
            Some(at) => at.saturating_add_signed(by).min(last),
        };
        self.select(selectable[next], cx);
    }

    /// Acts on the row at `index` as `how` says.
    pub fn activate(&mut self, index: usize, how: ChooserAction, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else { return };
        let event = match &entry.row {
            Row::Section { .. } => return,
            Row::Edit => ConfigChooserEvent::EditConfigs,
            Row::Save { config } => ConfigChooserEvent::SaveTemporary(config.id.clone()),
            // Current File has nothing to edit: it is only selected.
            Row::Current { .. } => ConfigChooserEvent::Chosen {
                id: CURRENT_FILE.to_owned(),
                how: if how == ChooserAction::Edit { ChooserAction::Select } else { how },
            },
            Row::Config { config, .. } => ConfigChooserEvent::Chosen { id: config.id.clone(), how },
        };
        cx.emit(event);
    }

    /// Activates the selected row as Enter does.
    pub fn enter(&mut self, shift: bool, alt_or_primary: bool, cx: &mut Context<Self>) {
        let Some(index) = self.active else { return };
        // The widget's chooser keeps Shift+Enter = Debug and Alt/⌘+Enter = Run.
        let how = match self.mode {
            ChooserMode::Select if shift => ChooserAction::Debug,
            ChooserMode::Select if alt_or_primary => ChooserAction::Run,
            ChooserMode::Select => ChooserAction::Select,
            mode if shift => alternate(mode),
            mode => action_of(mode),
        };
        self.activate(index, how, cx);
    }

    /// The keys the chooser handles itself while the keyboard is in its query. Returns whether
    /// `keystroke` is one of them.
    fn claim_key(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let modifiers = keystroke.modifiers;
        if self.shift != modifiers.shift {
            self.shift = modifiers.shift;
            cx.notify();
        }
        let plain = !modifiers.alt && !modifiers.control && !modifiers.platform;
        let key = keystroke.key.as_str();
        let config_row = self.active.and_then(|index| self.entries.get(index)).and_then(|entry| match &entry.row {
            Row::Config { config, .. } => Some(config.id.clone()),
            _ => None,
        });
        match key {
            "enter" => self.enter(modifiers.shift, modifiers.alt || modifiers.platform, cx),
            "up" => self.step(-1, true, cx),
            "down" => self.step(1, true, cx),
            "pageup" => self.step(-PAGE_ROWS, false, cx),
            "pagedown" => self.step(PAGE_ROWS, false, cx),
            "f4" => match (self.active, &config_row) {
                (Some(index), Some(_)) => self.activate(index, ChooserAction::Edit, cx),
                _ => return false,
            },
            "delete" if plain && !modifiers.shift => {
                let field = self.field.read(cx);
                let at_end = field.selection() == (field.text().len()..field.text().len());
                let Some(id) = config_row.filter(|id| at_end && self.store.read(cx).can_delete_config(id)) else {
                    return false;
                };
                cx.emit(ConfigChooserEvent::Delete(id));
            }
            digit if plain && digit.len() == 1 && matches!(digit.as_bytes()[0], b'1'..=b'9') => {
                let number = digit.as_bytes()[0] - b'0';
                let Some(index) = self.entries.iter().position(|entry| entry.number == Some(number)) else {
                    return false;
                };
                let how = if modifiers.shift { alternate(self.mode) } else { action_of(self.mode) };
                self.activate(index, how, cx);
            }
            _ => return false,
        }
        true
    }

    fn modifiers_changed(&mut self, event: &ModifiersChangedEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.shift != event.modifiers.shift {
            self.shift = event.modifiers.shift;
            cx.notify();
        }
    }

    fn inline_buttons(&self, entry: &Entry) -> Vec<InlineButton> {
        if !matches!(entry.row, Row::Config { .. }) {
            return Vec::new();
        }
        let mut buttons = vec![InlineButton::Pin];
        if entry.running.is_some() {
            buttons.extend([InlineButton::Rerun, InlineButton::Stop]);
        } else {
            buttons.push(InlineButton::Run);
            if self.debug_available {
                buttons.push(InlineButton::Debug);
            }
        }
        buttons
    }

    fn button_title(&self, entry: &Entry, button: InlineButton, cx: &App) -> String {
        let name = &entry.line;
        match button {
            InlineButton::Pin => tx(cx, if entry.pinned { "Unpin" } else { "Pin" }),
            InlineButton::Run => tx_template(cx, format!("Run '{name}'")),
            InlineButton::Debug => tx_template(cx, format!("Debug '{name}'")),
            InlineButton::Rerun => tx_template(cx, format!("Rerun '{name}'")),
            InlineButton::Stop => tx_template(cx, format!("Stop '{name}'")),
        }
    }

    /// Carries out an inline button of the row at `index`.
    fn press(&mut self, index: usize, button: InlineButton, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else { return };
        let Row::Config { config, .. } = &entry.row else { return };
        match (button, entry.running) {
            (InlineButton::Pin, _) => cx.emit(ConfigChooserEvent::TogglePinned(config.id.clone())),
            (InlineButton::Run, _) => self.activate(index, ChooserAction::Run, cx),
            (InlineButton::Debug, _) => self.activate(index, ChooserAction::Debug, cx),
            (InlineButton::Rerun, Some(session)) => cx.emit(ConfigChooserEvent::Rerun(session)),
            (InlineButton::Stop, Some(session)) => cx.emit(ConfigChooserEvent::Stop(session)),
            (InlineButton::Rerun | InlineButton::Stop, None) => {}
        }
    }

    fn render_row(&self, index: usize, entry: &Entry, active: bool, cx: &mut Context<Self>) -> AnyElement {
        let (fg, muted, faint) = (theme::color(cx, "fg"), theme::color(cx, "fg-subtle"), theme::color(cx, "fg-faint"));
        let strong = theme::color(cx, "fg-strong");
        let line = div()
            .id(index)
            .debug_selector(move || format!("run-chooser-row-{index}"))
            .h(px(ROW))
            .w_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_color(fg);
        let line = match &entry.row {
            Row::Section { label, .. } => {
                return line
                    .text_color(muted)
                    .text_size(theme::length(cx, "fs-small"))
                    .child(SharedString::from(tx(cx, label)))
                    .into_any_element();
            }
            Row::Save { config } => line
                .text_color(theme::color(cx, "fg-secondary"))
                .child(theme::icon(cx, "check", 13.0, muted))
                .child(SharedString::from(tx_template(cx, format!("Save '{}' Configuration", config.name)))),
            Row::Current { indices } => line.child(theme::icon(cx, "file", 13.0, muted)).child(
                marked_text(cx, &tx(cx, "Current File"), indices.as_deref())
                    .when(entry.selected, |el| el.text_color(strong)),
            ),
            Row::Edit => line
                .text_color(theme::color(cx, "fg-secondary"))
                .child(theme::icon(cx, "edit", 13.0, muted))
                .child(div().flex_1().child(SharedString::from(tx(cx, "Edit Configurations…"))))
                .children(self.edit_shortcut.clone().map(|shortcut| {
                    div().text_color(muted).text_size(theme::length(cx, "fs-small")).child(SharedString::from(shortcut))
                })),
            Row::Config { config, indices } => {
                let icon = config_icon(&config.kind, config.is_test, &self.types);
                let icon_color = if entry.temporary { faint } else { muted };
                let mark = entry.running.is_some().then(|| {
                    div()
                        .absolute()
                        .top(px(-1.0))
                        .right(px(-2.0))
                        .size(px(6.0))
                        .rounded(px(3.0))
                        .bg(theme::color(cx, "success"))
                });
                let buttons: Vec<AnyElement> = if active {
                    self.inline_buttons(entry)
                        .into_iter()
                        .map(|button| {
                            let (hover, color) = (theme::color(cx, "bg-hover"), theme::color(cx, "fg-muted"));
                            div()
                                .id(button.selector())
                                .debug_selector(move || button.selector().to_owned())
                                .size(px(18.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme::length(cx, "radius-small"))
                                .hover(move |el| el.bg(hover))
                                .child(theme::icon(cx, button.icon(), 12.0, color))
                                .on_click(cx.listener(move |chooser, _, _, cx| {
                                    cx.stop_propagation();
                                    chooser.press(index, button, cx);
                                }))
                                .into_any_element()
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let small = |text: &str| {
                    let text = SharedString::from(tx(cx, text));
                    div().text_color(faint).text_size(theme::length(cx, "fs-small")).child(text)
                };
                let numbered = self.entries.iter().any(|entry| entry.number.is_some());
                line.child(div().relative().flex_none().child(theme::icon(cx, &icon, 13.0, icon_color)).children(mark))
                    .when(numbered, |el| {
                        el.child(
                            div()
                                .w(px(8.0))
                                .flex_none()
                                .text_color(muted)
                                .text_size(theme::length(cx, "fs-small"))
                                .child(SharedString::from(entry.number.map(|n| n.to_string()).unwrap_or_default())),
                        )
                    })
                    .child(
                        marked_text(cx, &config.name, indices.as_deref())
                            .when(entry.selected, |el| el.text_color(strong)),
                    )
                    .when(config.is_test, |el| {
                        el.child(
                            div()
                                .px(px(4.0))
                                .rounded(theme::length(cx, "radius-small"))
                                .bg(theme::color(cx, "bg-control"))
                                .text_color(theme::color(cx, "fg-muted"))
                                .text_size(theme::length(cx, "fs-tiny"))
                                .child(SharedString::from(tx(cx, "test"))),
                        )
                    })
                    .when(!config.detected, |el| el.child(small("user")))
                    .when(entry.temporary, |el| el.child(small("temporary")))
                    .child(div().flex_1())
                    .children(buttons)
            }
        };
        let how = self.executor();
        line.when(active, |el| el.bg(theme::color(cx, "bg-tree-selected")))
            .on_mouse_move(cx.listener(move |chooser, _: &MouseMoveEvent, _, cx| {
                if chooser.active != Some(index) {
                    chooser.select(index, cx);
                }
            }))
            .on_click(cx.listener(move |chooser, _, _, cx| {
                chooser.select(index, cx);
                chooser.activate(index, how, cx);
            }))
            .into_any_element()
    }
}

impl Focusable for ConfigChooser {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.field.focus_handle(cx)
    }
}

impl Render for ConfigChooser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = Rc::clone(&self.entries);
        let active = self.active;
        let list = uniform_list(
            "run-chooser",
            entries.len(),
            cx.processor(move |chooser, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| chooser.render_row(index, &entries[index], active == Some(index), cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();
        let height = ROW * self.entries.len().clamp(1, VISIBLE_ROWS) as f32;
        let (muted, border) = (theme::color(cx, "fg-faint"), theme::color(cx, "border-popup"));
        div()
            .debug_selector(|| "run-chooser".to_owned())
            .w(px(WIDTH))
            .flex()
            .flex_col()
            .bg(theme::color(cx, "bg-popup"))
            .border_1()
            .border_color(border)
            .rounded(theme::length(cx, "radius-popup"))
            .overflow_hidden()
            .text_size(theme::length(cx, "fs-ui"))
            .on_modifiers_changed(cx.listener(Self::modifiers_changed))
            .children(self.caption(cx).map(|caption| {
                div()
                    .px(px(10.0))
                    .pt(px(6.0))
                    .text_color(theme::color(cx, "fg-secondary"))
                    .child(SharedString::from(caption))
            }))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(border)
                    .child(theme::icon(cx, "search", 14.0, theme::color(cx, "fg-muted")))
                    .child(div().flex_1().min_w_0().child(self.field.clone())),
            )
            .child(div().flex_none().h(px(height)).py(px(2.0)).child(list))
            .children(self.hint(cx).map(|hint| {
                div()
                    .px(px(10.0))
                    .py(px(4.0))
                    .border_t_1()
                    .border_color(border)
                    .text_color(muted)
                    .text_size(theme::length(cx, "fs-small"))
                    .child(SharedString::from(hint))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::configs::blank_config;
    use crate::run::store::{RunSession, SessionStatus};
    use crate::run::types::register_builtin_types;
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext};
    use ide_run::RunConfig;
    use std::cell::RefCell;
    mod proofs8_chooser;

    type Events = Rc<RefCell<Vec<ConfigChooserEvent>>>;

    fn types() -> Rc<RunConfigTypes> {
        let mut types = RunConfigTypes::new();
        register_builtin_types(&mut types);
        Rc::new(types)
    }

    fn config(id: &str, name: &str, kind: &str) -> RunConfig {
        RunConfig { command: "run".into(), detected: true, ..blank_config(id, name, kind) }
    }

    fn running(config: &RunConfig, id: &str, started_at: f64) -> RunSession {
        RunSession {
            id: id.to_owned(),
            config: config.clone(),
            run_id: None,
            status: SessionStatus::Running,
            exit_code: None,
            started_at,
            ended_at: None,
            stdin_closed: false,
            stopped: false,
            base_config: None,
            terminal: false,
            http_port: None,
        }
    }

    fn store_of(configs: Vec<RunConfig>, selected: Option<&str>) -> RunStore {
        let mut store = RunStore::new();
        store.set_configs(configs);
        store.loaded = true;
        store.selected_id = selected.map(str::to_owned);
        store
    }

    /// The chooser on `store` in a window of its own, with the keyboard in its query. Pins and
    /// deletions it asks for are applied to the store, as the workbench does.
    fn open(
        cx: &mut TestAppContext,
        store: RunStore,
        mode: ChooserMode,
    ) -> (Entity<ConfigChooser>, Entity<RunStore>, Events, &mut VisualTestContext) {
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            input::init(cx);
        });
        let store = cx.update(|cx| cx.new(|_| store));
        let shared = store.clone();
        let (chooser, cx) = cx.add_window_view(|_, cx| ConfigChooser::new(shared, types(), mode, cx));
        let events: Events = Rc::default();
        let (seen, applied) = (Rc::clone(&events), store.clone());
        cx.update(|window, cx| {
            cx.subscribe(&chooser, move |_, event: &ConfigChooserEvent, cx| {
                seen.borrow_mut().push(event.clone());
                applied.update(cx, |store, cx| {
                    match event {
                        ConfigChooserEvent::TogglePinned(id) => {
                            store.toggle_pinned(id);
                        }
                        ConfigChooserEvent::Delete(id) => {
                            store.delete_config(id);
                        }
                        _ => return,
                    }
                    cx.notify();
                });
            })
            .detach();
            let focus = chooser.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        (chooser, store, events, cx)
    }

    fn lines(chooser: &Entity<ConfigChooser>, cx: &mut VisualTestContext) -> Vec<String> {
        chooser.read_with(cx, |chooser, _| chooser.lines())
    }

    fn buttons(chooser: &Entity<ConfigChooser>, cx: &mut VisualTestContext) -> Vec<String> {
        chooser.read_with(cx, |chooser, cx| chooser.buttons(cx)).into_iter().map(|(title, _)| title).collect()
    }

    /// Selects the row `line` with the keyboard.
    fn move_to(chooser: &Entity<ConfigChooser>, line: &str, cx: &mut VisualTestContext) {
        for _ in 0..lines(chooser, cx).len() {
            if chooser.read_with(cx, |chooser, _| chooser.selected()).as_deref() == Some(line) {
                return;
            }
            cx.simulate_keystrokes("down");
        }
        panic!("{line} is not a row");
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    fn pin_fixture() -> RunStore {
        let configs =
            vec![config("build", "build", "npm"), config("serve", "serve", "npm"), config("App", "App", "application")];
        store_of(configs, None)
    }

    // src/features/run/chooserPinned.test.tsx › pins and unpins from the row's button
    #[gpui_kit::test]
    async fn pins_and_unpins_from_the_rows_button(cx: &mut TestAppContext) {
        let (chooser, store, _, cx) = open(cx, pin_fixture(), ChooserMode::Select);
        move_to(&chooser, "build", cx);
        assert_eq!(buttons(&chooser, cx)[0], "Pin");
        click(cx, "run-chooser-pin");
        assert_eq!(store.read_with(cx, |store, _| store.pinned.clone()), ["build"]);
        assert!(lines(&chooser, cx).contains(&"[Pinned]".to_owned()));
        // The selection stays on the pinned row, which now offers Unpin.
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.selected()).as_deref(), Some("build"));
        assert_eq!(buttons(&chooser, cx)[0], "Unpin");
        click(cx, "run-chooser-pin");
        assert!(store.read_with(cx, |store, _| store.pinned.is_empty()));
    }

    // src/features/run/chooserPinned.test.tsx › lists pinned configurations first, in pin order, and not
    // again in their group
    #[gpui_kit::test]
    async fn lists_pinned_configurations_first_in_pin_order_and_not_again_in_their_group(cx: &mut TestAppContext) {
        let mut store = pin_fixture();
        store.pinned = ["serve", "App", "gone"].map(str::to_owned).to_vec();
        let (chooser, _, _, cx) = open(cx, store, ChooserMode::Select);
        assert_eq!(lines(&chooser, cx), ["current", "[Pinned]", "serve", "App", "[npm]", "build", "edit"]);
        // A search keeps the pinned matches on top.
        cx.simulate_input("s");
        assert_eq!(lines(&chooser, cx), ["[Pinned]", "serve", "edit"]);
    }

    fn running_fixture() -> RunStore {
        let server = config("npm:server", "server", "npm");
        let worker = config("npm:worker", "worker", "npm");
        let idle = config("npm:idle", "idle", "npm");
        let mut store = store_of(vec![server.clone(), worker.clone(), idle], Some("npm:server"));
        store.sessions = vec![running(&server, "s1", 1.0), running(&worker, "s2", 2.0)];
        store
    }

    // src/features/run/chooserRunning.test.tsx › marks running and debugged configurations
    #[gpui_kit::test]
    async fn marks_running_and_debugged_configurations(cx: &mut TestAppContext) {
        let (chooser, _, _, cx) = open(cx, running_fixture(), ChooserMode::Run);
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.running()), ["server", "worker"]);
    }

    // src/features/run/chooserRunning.test.tsx › offers Rerun and Stop for a running configuration instead of
    // Run and Debug
    #[gpui_kit::test]
    async fn offers_rerun_and_stop_for_a_running_configuration_instead_of_run_and_debug(cx: &mut TestAppContext) {
        let (chooser, _, events, cx) = open(cx, running_fixture(), ChooserMode::Run);
        move_to(&chooser, "worker", cx);
        assert_eq!(buttons(&chooser, cx), ["Pin", "Rerun 'worker'", "Stop 'worker'"]);
        click(cx, "run-chooser-stop");
        assert_eq!(*events.borrow(), [ConfigChooserEvent::Stop("s2".into())]);
        click(cx, "run-chooser-rerun");
        assert_eq!(events.borrow().last(), Some(&ConfigChooserEvent::Rerun("s2".into())));
    }

    // src/features/run/chooserRunning.test.tsx › keeps Run and Debug for a configuration that isn't running
    #[gpui_kit::test]
    async fn keeps_run_and_debug_for_a_configuration_that_isnt_running(cx: &mut TestAppContext) {
        let (chooser, _, events, cx) = open(cx, running_fixture(), ChooserMode::Run);
        move_to(&chooser, "idle", cx);
        assert_eq!(buttons(&chooser, cx), ["Pin", "Run 'idle'", "Debug 'idle'"]);
        click(cx, "run-chooser-run");
        assert_eq!(*events.borrow(), [ConfigChooserEvent::Chosen { id: "npm:idle".into(), how: ChooserAction::Run }]);
    }

    // src/features/run/removal.test.tsx › chooser popup Delete removes the selected detection
    #[gpui_kit::test]
    async fn chooser_popup_delete_removes_the_selected_detection(cx: &mut TestAppContext) {
        let store = store_of(vec![config("npm:app", "Detected app", "npm")], Some("npm:app"));
        let (chooser, store, events, cx) = open(cx, store, ChooserMode::Select);
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.selected()).as_deref(), Some("Detected app"));
        cx.simulate_keystrokes("delete");
        assert_eq!(*events.borrow(), [ConfigChooserEvent::Delete("npm:app".into())]);
        // What deleting it saves: no user configuration, and the detection suppressed.
        let saved = store.update(cx, |store, _| store.delete_config("npm:app").1);
        assert_eq!(saved, Some((Vec::new(), vec!["npm:app".to_owned()])));
    }

    // src/features/run/runConfigTypes.test.tsx › Current File › is always offered in the chooser
    #[gpui_kit::test]
    async fn is_always_offered_in_the_chooser(cx: &mut TestAppContext) {
        let store = store_of(vec![config("a", "a", "shell")], Some(CURRENT_FILE));
        let (chooser, _, events, cx) = open(cx, store, ChooserMode::Select);
        assert_eq!(lines(&chooser, cx)[0], "current");
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.selected()).as_deref(), Some("current"));
        cx.simulate_input("cur");
        assert_eq!(lines(&chooser, cx), ["current", "edit"]);
        cx.simulate_keystrokes("enter");
        assert_eq!(
            *events.borrow(),
            [ConfigChooserEvent::Chosen { id: CURRENT_FILE.into(), how: ChooserAction::Select }]
        );
    }

    // Not from a spec: numbers pick a row while running, Shift switches to Debug, and the caption follows.
    #[gpui_kit::test]
    async fn numbers_run_the_nth_configuration_and_shift_debugs_it(cx: &mut TestAppContext) {
        let (chooser, _, events, cx) = open(cx, running_fixture(), ChooserMode::Run);
        let numbers = chooser.read_with(cx, |chooser, _| chooser.numbers());
        assert_eq!(numbers, [("server".to_owned(), 1), ("worker".to_owned(), 2), ("idle".to_owned(), 3)]);
        assert_eq!(chooser.read_with(cx, |chooser, cx| chooser.caption(cx)).as_deref(), Some("Run"));
        cx.simulate_keystrokes("3 shift-2");
        assert_eq!(
            *events.borrow(),
            [
                ConfigChooserEvent::Chosen { id: "npm:idle".into(), how: ChooserAction::Run },
                ConfigChooserEvent::Chosen { id: "npm:worker".into(), how: ChooserAction::Debug },
            ]
        );
        // Typing hides the numbers: digits are text then.
        cx.simulate_input("w");
        assert!(chooser.read_with(cx, |chooser, _| chooser.numbers()).is_empty());
    }

    // Not from a spec: the arrows skip section headers and go round, Enter runs, Shift+Enter debugs, F4 edits,
    // Enter on the last row opens Edit Configurations and Escape closes.
    #[gpui_kit::test]
    async fn the_keyboard_moves_over_rows_and_acts_on_them(cx: &mut TestAppContext) {
        let (chooser, _, events, cx) = open(cx, running_fixture(), ChooserMode::Run);
        let selected = |cx: &mut VisualTestContext| chooser.read_with(cx, |chooser, _| chooser.selected());
        assert_eq!(selected(cx).as_deref(), Some("server"));
        cx.simulate_keystrokes("up");
        assert_eq!(selected(cx).as_deref(), Some("current"));
        cx.simulate_keystrokes("up");
        assert_eq!(selected(cx).as_deref(), Some("edit"));
        cx.simulate_keystrokes("down down");
        assert_eq!(selected(cx).as_deref(), Some("server"));
        cx.simulate_keystrokes("enter shift-enter f4");
        cx.simulate_keystrokes("up up enter escape");
        let id = "npm:server".to_owned();
        assert_eq!(
            *events.borrow(),
            [
                ConfigChooserEvent::Chosen { id: id.clone(), how: ChooserAction::Run },
                ConfigChooserEvent::Chosen { id: id.clone(), how: ChooserAction::Debug },
                ConfigChooserEvent::Chosen { id, how: ChooserAction::Edit },
                ConfigChooserEvent::EditConfigs,
                ConfigChooserEvent::Dismissed,
            ]
        );
    }

    // Not from a spec: a selected temporary configuration offers to be saved first, and the widget's
    // chooser selects on Enter and runs on ⌘Enter, without numbers or caption.
    #[gpui_kit::test]
    async fn offers_to_save_a_selected_temporary_configuration(cx: &mut TestAppContext) {
        let mut store = store_of(vec![config("a", "a", "shell"), config("tmp", "Main", "application")], Some("tmp"));
        store.temporary = vec!["tmp".to_owned()];
        let (chooser, _, events, cx) = open(cx, store, ChooserMode::Select);
        assert_eq!(lines(&chooser, cx)[0], "save");
        assert_eq!(chooser.read_with(cx, |chooser, cx| (chooser.caption(cx), chooser.numbers())), (None, Vec::new()));
        move_to(&chooser, "save", cx);
        cx.simulate_keystrokes("enter");
        move_to(&chooser, "a", cx);
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(
            *events.borrow(),
            [
                ConfigChooserEvent::SaveTemporary("tmp".into()),
                ConfigChooserEvent::Chosen { id: "a".into(), how: ChooserAction::Run },
            ]
        );
    }
}
