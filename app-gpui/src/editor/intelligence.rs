//! Code intelligence of one editor: completion, navigation, hover with the problems under the
//! pointer, parameter info, the problems underlined in the text, and the hints at the caret.
//!
//! The workbench gives every editor it opens one [`EditorIntelligence`], which the editor draws as
//! its overlay and so lives exactly as long as the editor. It follows the editor's edits and caret,
//! sends the requests to the backend off the main thread, drops the answers of requests that a newer
//! one superseded, and draws its popups over the text without taking the keyboard: the lookup claims
//! its keys on the editor's focus handle.

use super::buffer;
use super::marks::{Mark, MarkKind, MarkLayer};
use super::view::{EditOrigin, Editor, EditorEvent, PointerHandler};
use crate::backend::Backend;
use crate::editor_lsp::completion::{
    AUTO_INSERT_DELAY_MS, Answered, ApplyMode, ApplyOptions, Ask, CompletionRequest, CompletionSource, FollowUp,
    LookupEvent, MatchCase, NO_SUGGESTIONS, NO_SUGGESTIONS_TIMEOUT_MS, apply_contributed, apply_lsp_completion,
    completion_options, completion_query, late_edits, reuses_rows, rows_valid_for,
};
use crate::editor_lsp::completion_view::{CompletionEvent, CompletionLookup, LookupItem, LookupOptions};
use crate::editor_lsp::configuration_navigation::navigation_word;
use crate::editor_lsp::diagnostics::{DiagnosticStore, LintDiagnostic, diagnostics_at, to_lint_diagnostics};
use crate::editor_lsp::hint::{HintState, HintTimer, PENDING_DELAY_MS, Tone};
use crate::editor_lsp::hover::{self, NavigationModifier, PointerTarget, hover_popup};
use crate::editor_lsp::hover_view::{HintView, HoverEvent, HoverView};
use crate::editor_lsp::intentions::{
    IntentionItem, IntentionRun, PreviewStep, ResolveCache, action_items, listed_code_actions, listed_intentions,
    quick_fix_icon, with_inspection_controls,
};
use crate::editor_lsp::intentions_view::{ListPopup, ListPopupEvent, intentions_popup};
use crate::editor_lsp::navigation::{
    Chooser, GotoFlow, GotoStep, PointerNavigation, SHOW_USAGES_ACTION, Target, pointer_navigation,
};
use crate::editor_lsp::positions::offset_to_pos;
use crate::editor_lsp::signature::{NO_PARAMETERS, REFRESH_MS, SignatureFollowUp, SignatureSession};
use crate::editor_lsp::signature_view::{ParameterInfo, ParameterInfoEvent};
use crate::editor_lsp::snippet::SnippetClock;
use crate::editor_lsp::targets_view::TargetsChooser;
use crate::platform::i18n::Localization;
use crate::platform::input;
use crate::settings_model::inspections_model::ProfileOverrides;
use crate::theme;
use gpui_kit::{
    Anchor, App, AppContext, Context, Entity, EventEmitter, Focusable, IntoElement, MouseDownEvent, Render,
    Subscription, Task, WeakEntity, Window, anchored, deferred, div, point, prelude::*,
};
use ide_core::Severity;
use ide_core::ipc_path::to_ipc;
use ide_core::{DocId, IdeError, Location, RequestId, TextChange};
use ide_lsp::types::CodeAction;
use ide_lsp::types::GotoKind;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the controller asks of the window it is in.
#[derive(Debug, Clone)]
pub enum IntelligenceEvent {
    /// Open `location` with the caret at its start (a navigation target).
    Open(Location),
    /// Show the targets chooser as the window's popup.
    Choose(Entity<TargetsChooser>),
    /// Run the action `id` in the window.
    RunAction(&'static str),
    /// Show the intentions list as the window's popup.
    Intentions(Entity<ListPopup>),
}

/// The code intelligence of one editor.
pub struct EditorIntelligence {
    editor: WeakEntity<Editor>,
    doc: DocId,
    window_label: String,
    path: String,
    language: String,
    lookup: Entity<CompletionLookup>,
    /// Where the word the lookup completes starts.
    lookup_from: u32,
    /// The rows came from an incomplete list: typing asks again.
    lookup_incomplete: bool,
    /// The completion that is being asked: its word start and whether it was explicit.
    asking: Option<(u32, bool)>,
    /// Completion that typing starts, waiting for the typing to pause.
    activation: Option<Task<()>>,
    source: CompletionSource,
    hints: HintState,
    hint_view: Entity<HintView>,
    goto: Option<GotoFlow>,
    chooser: Option<WeakEntity<TargetsChooser>>,
    /// When the document last changed, for navigation's retry after an edit.
    edited: Option<Instant>,
    started: Instant,
    key_claim: u64,
    params: Entity<ParameterInfo>,
    signature: SignatureSession,
    hover: Entity<HoverView>,
    /// The range the hover shows for, while it shows.
    hover_range: Option<(u32, u32)>,
    /// Bumped by every pointer move and hover request: a later rest or answer wins.
    hover_ticket: u64,
    hover_request: Option<RequestId>,
    /// The pointer is over the hover popup, which keeps it open.
    pointer_in_hover: bool,
    navigation_modifier: NavigationModifier,
    diagnostics: DiagnosticStore,
    /// The problems of the document, in the coordinates of its current text.
    lint: Vec<LintDiagnostic>,
    listener: u64,
    /// The intentions list while it is open: its rows, the code actions they run and the actions
    /// resolved for their previews.
    intentions: Option<OpenIntentions>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<IntelligenceEvent> for EditorIntelligence {}

impl EditorIntelligence {
    /// The intelligence of `editor`, which then draws it over its text.
    pub fn install(editor: &Entity<Editor>, cx: &mut App) -> Entity<Self> {
        let intelligence = cx.new(|cx| Self::new(editor, cx));
        let pointer = pointer_handler(intelligence.downgrade());
        editor.update(cx, |editor, cx| {
            editor.set_pointer_handler(Some(pointer));
            editor.set_overlay(Some(intelligence.clone().into()), cx);
        });
        intelligence
    }

    /// The intelligence the workbench installed on `editor`.
    pub fn of(editor: &Entity<Editor>, cx: &App) -> Option<Entity<Self>> {
        editor.read(cx).overlay().and_then(|overlay| overlay.clone().downcast::<Self>().ok())
    }

    fn new(editor: &Entity<Editor>, cx: &mut Context<Self>) -> Self {
        let focus = editor.focus_handle(cx);
        // Claims go in order of creation: Escape closes parameter info, then the lookup, then the
        // hover or the hint, before the editor's own handler sees it.
        let params = cx.new(|cx| ParameterInfo::new(focus.clone(), cx));
        let lookup = cx.new(|cx| CompletionLookup::new(focus.clone(), cx));
        let claimant = cx.weak_entity();
        let key_claim = input::claim_keys(
            focus,
            move |keystroke, _, cx| {
                let plain = !keystroke.modifiers.modified();
                keystroke.key == "escape" && plain && claimant.update(cx, |this, cx| this.escape(cx)).unwrap_or(false)
            },
            cx,
        );
        cx.on_release(|this, cx| {
            input::release_keys(this.key_claim, cx);
            if cx.has_global::<Backend>() {
                cx.global_mut::<Backend>().unlisten(this.listener);
            }
        })
        .detach();
        let hover = cx.new(HoverView::new);
        let subscriptions = vec![
            cx.subscribe(editor, |this, editor, event: &EditorEvent, cx| this.editor_event(&editor, event, cx)),
            cx.subscribe(&lookup, |this, _, event: &CompletionEvent, cx| this.lookup_event(event, cx)),
            cx.subscribe(&params, |this, _, event: &ParameterInfoEvent, cx| match event {
                ParameterInfoEvent::Closed => {
                    this.signature.close();
                    cx.notify();
                }
            }),
            cx.subscribe(&hover, |this, _, event: &HoverEvent, cx| this.hover_event(event, cx)),
        ];
        let path = to_ipc(editor.read(cx).path());
        let listener = {
            let (this, path) = (cx.weak_entity(), path.clone());
            cx.global_mut::<Backend>().listen(move |event, cx| {
                if let ide_core::IdeEvent::Diagnostics { path: published, source, items } = &event.event
                    && to_ipc(&published.0) == path
                {
                    let _ = this.update(cx, |this, cx| this.diagnostics_published(source, items.clone(), cx));
                }
            })
        };
        // What the servers published before the editor opened comes again.
        let label = editor.read(cx).window_label().to_owned();
        let resend = cx
            .global::<Backend>()
            .with_host(move |host| async move { ide_host::ops::lsp::lsp_resend_diagnostics(&host, &label) });
        cx.background_executor()
            .spawn(async move {
                match resend.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::debug!("no diagnostics to resend: {}", error.message),
                    Err(error) => tracing::warn!("resending the diagnostics did not run: {error:#}"),
                }
            })
            .detach();
        let read = editor.read(cx);
        Self {
            editor: editor.downgrade(),
            doc: read.doc(),
            window_label: read.window_label().to_owned(),
            path,
            language: read.language().as_str().to_owned(),
            lookup,
            lookup_from: 0,
            lookup_incomplete: false,
            asking: None,
            activation: None,
            source: CompletionSource::default(),
            hints: HintState::default(),
            hint_view: cx.new(|_| HintView::default()),
            goto: None,
            chooser: None,
            edited: None,
            started: Instant::now(),
            key_claim,
            params,
            signature: SignatureSession::default(),
            hover,
            hover_range: None,
            hover_ticket: 0,
            hover_request: None,
            pointer_in_hover: false,
            navigation_modifier: NavigationModifier::new(crate::platform::keymap::is_mac_host()),
            diagnostics: DiagnosticStore::default(),
            lint: Vec::new(),
            listener,
            intentions: None,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn parameter_info(&self) -> &Entity<ParameterInfo> {
        &self.params
    }

    pub fn hover(&self) -> &Entity<HoverView> {
        &self.hover
    }

    /// The view drawing the hint at the caret.
    pub fn hint_view(&self) -> &Entity<HintView> {
        &self.hint_view
    }

    pub fn lookup(&self) -> &Entity<CompletionLookup> {
        &self.lookup
    }

    /// The targets chooser while it is open.
    pub fn chooser(&self) -> Option<Entity<TargetsChooser>> {
        self.chooser.as_ref().and_then(WeakEntity::upgrade)
    }

    /// The hint showing at the caret, in English source text.
    pub fn hint(&self) -> Option<&str> {
        self.hints.hint().map(|hint| hint.text.as_str())
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Runs `task` for as long as the controller lives; finished ones are dropped on the way.
    fn keep(&mut self, task: Task<()>) {
        self._tasks.retain(|task| !task.is_ready());
        self._tasks.push(task);
    }

    fn settings(&self, cx: &App) -> ide_core::editor_settings::CompletionSettings {
        crate::platform::settings::effective(&self.window_label, cx).editor.completion
    }

    fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.hover_range.is_some() {
            self.close_hover(cx);
            return true;
        }
        let hidden = self.hints.hide();
        if hidden {
            self.show_hint(cx);
        }
        hidden
    }

    fn editor_event(&mut self, editor: &Entity<Editor>, event: &EditorEvent, cx: &mut Context<Self>) {
        match event {
            EditorEvent::Edited { origin, changes } => {
                self.edited = Some(Instant::now());
                self.hints.changed();
                self.show_hint(cx);
                self.close_hover(cx);
                self.follow_diagnostics(changes, editor, cx);
                let typed = match origin {
                    EditOrigin::Typed(text) => text.chars().last(),
                    _ => None,
                };
                self.signature_follow(changes, false, typed, cx);
                match origin {
                    EditOrigin::Typed(text) => self.typed(editor, text, cx),
                    EditOrigin::Deleted if self.lookup.read(cx).is_open() || self.asking.is_some() => {
                        self.follow_word(editor, cx);
                    }
                    EditOrigin::Completion => {}
                    _ => self.close_lookup(cx),
                }
            }
            EditorEvent::SelectionMoved => {
                self.hints.changed();
                self.show_hint(cx);
                self.close_hover(cx);
                self.signature_follow(&[], true, None, cx);
                self.activation = None;
                let caret = editor.read(cx).caret();
                if (self.lookup.read(cx).is_open() || self.asking.is_some()) && !self.word_holds(editor, caret, cx) {
                    self.close_lookup(cx);
                }
            }
            &EditorEvent::Hovered { offset, modifiers } => self.pointer_rests(offset, modifiers, cx),
            EditorEvent::GutterPressed { .. } => {}
        }
    }

    /// Whether the caret is still on the word the lookup completes.
    fn word_holds(&self, editor: &Entity<Editor>, caret: u32, cx: &App) -> bool {
        let from = self.asking.map_or(self.lookup_from, |(from, _)| from);
        caret >= from && self.query_at(editor, true, cx).is_some_and(|query| query.from == from)
    }

    /// The completion query at the main caret.
    fn query_at(
        &self,
        editor: &Entity<Editor>,
        explicit: bool,
        cx: &App,
    ) -> Option<crate::editor_lsp::completion::CompletionQuery> {
        let editor = editor.read(cx);
        let caret = editor.caret();
        let rope = editor.rope();
        let (line, _) = buffer::line_col(rope, caret);
        let before = buffer::slice(rope, buffer::line_range(rope, line).start, caret);
        completion_query(&before, caret, explicit)
    }

    /// Typing: an open lookup follows the word, or completion starts after a word character or a
    /// trigger character when Show suggestions as you type is on.
    fn typed(&mut self, editor: &Entity<Editor>, text: &str, cx: &mut Context<Self>) {
        if editor.read(cx).selections().len() > 1 || text.chars().any(|c| c == '\n' || c == '\r') {
            self.close_lookup(cx);
            return;
        }
        if self.lookup.read(cx).is_open() || self.asking.is_some() {
            self.follow_word(editor, cx);
            return;
        }
        if self.settings(cx).auto_popup && self.query_at(editor, false, cx).is_some() {
            self.activate_after_typing(cx);
        } else {
            self.activation = None;
        }
    }

    /// Starts completion once typing pauses for `ACTIVATE_ON_TYPING_DELAY_MS`: every keystroke
    /// before that restarts the wait, so a word typed in one go asks the server once, at its end.
    fn activate_after_typing(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(ACTIVATE_ON_TYPING_DELAY_MS)).await;
            let _ = this.update(cx, |this, cx| {
                this.activation = None;
                this.complete(false, cx);
            });
        });
        self.activation = Some(task);
    }

    /// The word before the caret changed while the lookup is open or asked for: the rows are
    /// filtered again when they stay valid, else completion asks again; without a word (or past
    /// it) the lookup closes.
    fn follow_word(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(query) = self.query_at(editor, false, cx) else {
            self.close_lookup(cx);
            return;
        };
        let explicit = self.asking.is_some_and(|(_, explicit)| explicit) || self.lookup.read(cx).focused();
        let same_word = query.from == self.lookup_from && query.trigger.is_none();
        let match_case = match_case(&self.settings(cx));
        if self.lookup.read(cx).is_open()
            && same_word
            && reuses_rows(self.lookup_incomplete, match_case)
            && rows_valid_for(&query.typed)
        {
            let event = self.lookup.update(cx, |lookup, cx| lookup.set_query(&query.typed, cx));
            self.lookup_event_result(event, cx);
            if !self.lookup.read(cx).is_open() {
                self.abort_completion(cx);
            }
            return;
        }
        self.complete(explicit, cx);
    }

    /// Asks the server for completion at the caret; `explicit` is Code › Code Completion › Basic.
    pub fn complete(&mut self, explicit: bool, cx: &mut Context<Self>) {
        self.activation = None;
        let Some(editor) = self.editor.upgrade() else { return };
        let Some(query) = self.query_at(&editor, explicit, cx) else { return };
        let id = Backend::next_request_id();
        let now = self.now_ms();
        if explicit {
            self.lookup.update(cx, |lookup, cx| {
                lookup.explicit(true);
                lookup.loading(cx);
            });
        }
        match self.source.ask(self.doc, id, now) {
            Ask::Fallback => {
                self.asking = None;
                if explicit {
                    let event = self.lookup.update(cx, |lookup, cx| lookup.close(false, cx));
                    self.lookup_event_result(event, cx);
                }
                return;
            }
            Ask::Request { cancel, .. } => {
                if let Some(cancel) = cancel {
                    cx.global::<Backend>().host.cancel_request(cancel);
                }
            }
        }
        self.asking = Some((query.from, explicit));
        let (rope, caret) = {
            let editor = editor.read(cx);
            (editor.rope().clone(), editor.caret())
        };
        let request = Arc::new(CompletionRequest {
            doc: self.doc,
            path: self.path.clone(),
            language: Some(self.language.clone()),
            pos: caret,
            text: rope.clone(),
        });
        let (window, doc, pos) = (self.window_label.clone(), self.doc, offset_to_pos(&rope, caret));
        let trigger = query.trigger.map(String::from);
        let work = cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::lsp::lsp_completion(&host, &window, id, doc, pos, trigger).await
        });
        let task = cx.spawn(async move |this, cx| {
            let answer = work.await.unwrap_or_else(|error| Err(IdeError::internal(error.to_string())));
            let _ = this.update(cx, |this, cx| this.completion_answered(id, request, query.from, answer, cx));
        });
        self.keep(task);
    }

    fn completion_answered(
        &mut self,
        id: RequestId,
        request: Arc<CompletionRequest>,
        from: u32,
        answer: Result<ide_lsp::types::CompletionList, IdeError>,
        cx: &mut Context<Self>,
    ) {
        let now = self.now_ms();
        let answered = self.source.answer(self.doc, id, answer, now);
        if matches!(answered, Answered::Stale) {
            return;
        }
        self.asking = None;
        let Some(editor) = self.editor.upgrade() else { return };
        let list = match answered {
            Answered::List(list) => list,
            Answered::Stale => return,
            Answered::Fallback | Answered::Nothing => {
                let event = self.lookup.update(cx, |lookup, cx| lookup.close(false, cx));
                self.lookup_event_result(event, cx);
                return;
            }
        };
        let Some(query) = self.query_at(&editor, true, cx).filter(|query| query.from == from) else {
            self.close_lookup(cx);
            return;
        };
        let settings = self.settings(cx);
        self.lookup_from = from;
        self.lookup_incomplete = list.is_incomplete;
        let items = completion_options(list, &request, &query.typed, match_case(&settings), &[]);
        let items = items.into_iter().map(|item| LookupItem::Lsp(Box::new(item))).collect();
        let options = LookupOptions {
            insert_on_special_keys: settings.insert_on_special_keys,
            show_documentation: settings.show_documentation,
            sort_alphabetically: settings.sort_alphabetically,
            auto_insert_single: settings.auto_insert_single,
        };
        let event = self.lookup.update(cx, |lookup, cx| {
            lookup.set_options(options, cx);
            lookup.set_items(items, &query.typed, cx)
        });
        self.lookup_event_result(event, cx);
    }

    /// Carries out what the lookup answered.
    fn lookup_event_result(&mut self, event: LookupEvent, cx: &mut Context<Self>) {
        match event {
            LookupEvent::None => {}
            LookupEvent::NoSuggestions => {
                let caret = self.editor.upgrade().map_or(0, |editor| editor.read(cx).caret());
                let timer = self.hints.show(NO_SUGGESTIONS, caret, Tone::Info, Some(NO_SUGGESTIONS_TIMEOUT_MS));
                self.hint_timer(timer, cx);
            }
            LookupEvent::AutoInsert => {
                let version = self.editor.upgrade().map(|editor| editor.read(cx).version());
                let task = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(AUTO_INSERT_DELAY_MS)).await;
                    let _ = this.update(cx, |this, cx| {
                        let now = this.editor.upgrade().map(|editor| editor.read(cx).version());
                        this.lookup.update(cx, |lookup, cx| lookup.auto_insert(now == version, cx));
                    });
                });
                self.keep(task);
            }
        }
    }

    fn abort_completion(&mut self, cx: &mut Context<Self>) {
        self.asking = None;
        self.activation = None;
        if let Some(id) = self.source.abort() {
            cx.global::<Backend>().host.cancel_request(id);
        }
    }

    fn close_lookup(&mut self, cx: &mut Context<Self>) {
        self.abort_completion(cx);
        if self.lookup.read(cx).is_open() {
            self.lookup.update(cx, |lookup, cx| lookup.close(true, cx));
        }
    }

    fn lookup_event(&mut self, event: &CompletionEvent, cx: &mut Context<Self>) {
        match event {
            CompletionEvent::Accept { item, mode, typed } => self.accept(item, *mode, *typed, cx),
            CompletionEvent::Closed => self.abort_completion(cx),
            CompletionEvent::Resolve { row, completion } => {
                let (row, item) = (*row, completion.item.clone());
                let (window, doc) = (self.window_label.clone(), self.doc);
                let work = cx.global::<Backend>().with_host(move |host| async move {
                    ide_host::ops::lsp::lsp_resolve_completion(&host, &window, doc, item).await
                });
                let task = cx.spawn(async move |this, cx| match work.await {
                    Ok(Ok(resolved)) => {
                        let _ = this.update(cx, |this, cx| {
                            this.lookup.update(cx, |lookup, cx| lookup.set_resolved(row, resolved, cx));
                        });
                    }
                    Ok(Err(error)) => tracing::debug!("completion item not resolved: {}", error.message),
                    Err(error) => tracing::warn!("completion resolve did not run: {error:#}"),
                });
                self.keep(task);
            }
            CompletionEvent::ToggleSortByName => {
                let sorted = !self.settings(cx).sort_alphabetically;
                let key = "editor.completion.sortAlphabetically";
                let written = crate::platform::settings::set(
                    &self.window_label,
                    ide_workspace::SettingsScope::Global,
                    key,
                    &serde_json::Value::Bool(sorted),
                    cx,
                );
                if let Err(error) = written {
                    tracing::warn!("cannot write {key}: {}", error.message);
                }
                self.complete(true, cx);
            }
        }
    }

    /// Puts the accepted item in as one edit (its additional edits with it), then types the commit
    /// character that finished the lookup and carries out the item's follow-ups.
    fn accept(&mut self, item: &LookupItem, mode: ApplyMode, typed: Option<char>, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        self.lookup.update(cx, |lookup, cx| lookup.close(true, cx));
        self.abort_completion(cx);
        let (rope, caret, indent_unit) = {
            let editor = editor.read(cx);
            (editor.rope().clone(), editor.caret(), editor.facts().indent_unit.clone())
        };
        let from = self.lookup_from.min(caret);
        let insert_parentheses = self.settings(cx).insert_parentheses;
        let applied = match item {
            LookupItem::Lsp(completion) => {
                let options = ApplyOptions { mode, insert_parentheses, indent_unit: &indent_unit, clock: clock() };
                apply_lsp_completion(completion, &rope, from, caret, options)
            }
            LookupItem::Contributed(item) => apply_contributed(&rope, from, caret, &item.apply, &indent_unit),
        };
        let selections = applied.selection.iter().map(|range| (range.start, range.end)).collect();
        let done = editor.update(cx, |editor, cx| {
            editor.apply_edit_from(EditOrigin::Completion, &applied.changes, selections, 0, cx)
        });
        if !done {
            return;
        }
        let opened_call = applied.changes.iter().any(|change| change.insert.contains('('));
        if let Some(ch) = typed.filter(|ch| *ch != '(' || !opened_call) {
            editor.update(cx, |editor, cx| editor.insert(None, &ch.to_string(), cx));
        }
        let inserted_at = applied.changes.iter().map(|change| change.from).max().unwrap_or(from);
        for follow_up in applied.follow_ups {
            match follow_up {
                FollowUp::Suggest => self.complete(true, cx),
                FollowUp::ExecuteCommand(command) => {
                    let (window, doc) = (self.window_label.clone(), self.doc);
                    let work = cx.global::<Backend>().with_host(move |host| async move {
                        ide_host::ops::lsp::lsp_execute_command(&host, &window, doc, command).await
                    });
                    let task = cx.spawn(async move |_, _| match work.await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!("completion command failed: {}", error.message),
                        Err(error) => tracing::warn!("completion command did not run: {error:#}"),
                    });
                    self.keep(task);
                }
                FollowUp::ResolveLateEdits { .. } => {
                    if let LookupItem::Lsp(completion) = item {
                        self.resolve_late_edits(completion.as_ref().clone(), inserted_at, cx);
                    }
                }
                FollowUp::SignatureHelp => self.ask_signature(false, cx),
                FollowUp::ParameterInfoOrLeave(inside) => self.parameter_info_or_leave(inside, cx),
            }
        }
    }

    /// The edits the server computes only on resolve (an auto-import) go in once it answered.
    fn resolve_late_edits(
        &mut self,
        completion: crate::editor_lsp::completion::LspCompletion,
        inserted_at: u32,
        cx: &mut Context<Self>,
    ) {
        let (window, doc, item) = (self.window_label.clone(), self.doc, completion.item.clone());
        let work = cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::lsp::lsp_resolve_completion(&host, &window, doc, item).await
        });
        let task = cx.spawn(async move |this, cx| {
            let resolved = match work.await {
                Ok(Ok(resolved)) => resolved,
                Ok(Err(error)) => return tracing::debug!("no late completion edits: {}", error.message),
                Err(error) => return tracing::warn!("completion resolve did not run: {error:#}"),
            };
            let _ = this.update(cx, |this, cx| {
                let Some(editor) = this.editor.upgrade() else { return };
                editor.update(cx, |editor, cx| {
                    let changes: Vec<TextChange> = late_edits(&completion, &resolved, editor.rope(), inserted_at);
                    let shift: i64 = changes
                        .iter()
                        .map(|change| i64::from(utf16_len(&change.insert)) - i64::from(change.to - change.from))
                        .sum();
                    let selections = editor
                        .selections()
                        .iter()
                        .map(|&(anchor, head)| (shifted(anchor, shift), shifted(head, shift)))
                        .collect();
                    let main = editor.main_index();
                    editor.apply_edit_from(EditOrigin::Completion, &changes, selections, main, cx);
                });
            });
        });
        self.keep(task);
    }

    /// Shows the hint state's hint, and hides it when its timer ends.
    fn hint_timer(&mut self, timer: HintTimer, cx: &mut Context<Self>) {
        self.show_hint(cx);
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(timer.delay_ms)).await;
            let _ = this.update(cx, |this, cx| {
                this.hints.timed_out(timer.ticket);
                this.show_hint(cx);
            });
        });
        self.keep(task);
    }

    fn show_hint(&mut self, cx: &mut Context<Self>) {
        let hint = self.hints.hint().cloned();
        self.hint_view.update(cx, |view, cx| view.set_hint(hint.as_ref(), cx));
        cx.notify();
    }

    fn show_hint_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let caret = self.editor.upgrade().map_or(0, |editor| editor.read(cx).caret());
        let timer = self.hints.show(text, caret, Tone::Info, None);
        self.hint_timer(timer, cx);
    }

    /// Shows `text` (English source text) at `pos`, the caret when `None`, with `tone`, for
    /// `timeout_ms` (the hint's usual time when `None`).
    pub fn show_hint_at(
        &mut self,
        text: &str,
        pos: Option<u32>,
        tone: Tone,
        timeout_ms: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let caret = self.editor.upgrade().map_or(0, |editor| editor.read(cx).caret());
        let timer = self.hints.show(text, pos.unwrap_or(caret), tone, timeout_ms);
        self.hint_timer(timer, cx);
    }

    // -----------------------------------------------------------------------------------------
    // Navigation

    /// Go to Declaration (`Definition`), Implementation or Type Declaration from the caret.
    pub fn goto(&mut self, kind: GotoKind, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        self.close_lookup(cx);
        let (rope, caret) = {
            let editor = editor.read(cx);
            (editor.rope().clone(), editor.caret())
        };
        let pos = offset_to_pos(&rope, caret);
        let name = navigation_word(&rope, caret, false).map(|word| buffer::slice(&rope, word.start, word.end));
        let mut flow = GotoFlow::new(kind, &self.path, pos, name, None);
        let id = Backend::next_request_id();
        flow.start(&self.hints, caret, id);
        self.goto = Some(flow);
        let progress = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(PENDING_DELAY_MS)).await;
            let _ = this.update(cx, |this, cx| {
                let timer = this.goto.as_mut().and_then(|flow| flow.progress_due(&mut this.hints));
                if let Some(timer) = timer {
                    this.hint_timer(timer, cx);
                }
            });
        });
        self.keep(progress);
        self.ask_goto(id, kind, pos, cx);
    }

    fn ask_goto(&mut self, id: RequestId, kind: GotoKind, pos: ide_core::Pos, cx: &mut Context<Self>) {
        let (window, doc) = (self.window_label.clone(), self.doc);
        let work = cx.global::<Backend>().with_host(move |host| async move {
            let cancel = host.begin_request(id);
            let found = tokio::select! {
                found = ide_host::ops::lsp::lsp_goto(&host, &window, doc, pos, kind) => found,
                _ = cancel.cancelled() => Err(IdeError::cancelled()),
            };
            host.end_request(id);
            found
        });
        let task = cx.spawn(async move |this, cx| {
            let answer = work.await.unwrap_or_else(|error| Err(IdeError::internal(error.to_string())));
            let _ = this.update(cx, |this, cx| {
                let since_edit = this.edited.map(|at| at.elapsed().as_millis() as u64);
                let Some(flow) = this.goto.as_mut() else { return };
                let i18n = &mut cx.default_global::<Localization>().0;
                let step = flow.answered(id, answer, since_edit, &mut this.hints, i18n);
                this.goto_step(step, kind, pos, cx);
            });
        });
        self.keep(task);
    }

    fn goto_step(&mut self, step: GotoStep, kind: GotoKind, pos: ide_core::Pos, cx: &mut Context<Self>) {
        self.show_hint(cx);
        match step {
            GotoStep::Nothing => {}
            GotoStep::Retry { delay_ms } => {
                let task = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(delay_ms)).await;
                    let _ = this.update(cx, |this, cx| {
                        let id = Backend::next_request_id();
                        if let Some(flow) = this.goto.as_mut() {
                            flow.retry(id);
                            this.ask_goto(id, kind, pos, cx);
                        }
                    });
                });
                self.keep(task);
            }
            GotoStep::Go { target, .. } => cx.emit(IntelligenceEvent::Open(target)),
            GotoStep::Chooser(chooser) => self.choose(chooser, cx),
            GotoStep::Hint(text) | GotoStep::References { otherwise: text } => self.show_hint_text(&text, cx),
            GotoStep::Fallback => self.syntax_fallback(pos, cx),
            GotoStep::ShowUsages => cx.emit(IntelligenceEvent::RunAction(SHOW_USAGES_ACTION)),
        }
    }

    /// Go to Declaration without an answer from a server: the local definition the syntax tree
    /// resolves the name at the caret to (several when the scope declares it more than once).
    fn syntax_fallback(&mut self, pos: ide_core::Pos, cx: &mut Context<Self>) {
        let (window, doc, path) = (self.window_label.clone(), self.doc, self.path.clone());
        let work = cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::syntax::syntax_locals(&host, &window, doc, pos, None).await
        });
        let task = cx.spawn(async move |this, cx| {
            let targets = match work.await {
                Ok(Ok(ide_syntax::Locals::Local { symbol })) => {
                    let mut ranges = vec![symbol.definition];
                    if !symbol.certain {
                        ranges.extend(symbol.conflicts);
                    }
                    ranges.sort_by_key(|range| (range.start.line, range.start.col));
                    ranges.dedup();
                    let location = |range| Location::file(std::path::PathBuf::from(&path), range);
                    Some(ranges.into_iter().map(|range| Target::from(location(range))).collect())
                }
                Ok(Ok(_)) => Some(Vec::new()),
                Ok(Err(error)) => {
                    tracing::debug!("no syntax fallback: {}", error.message);
                    None
                }
                Err(error) => {
                    tracing::warn!("the syntax fallback did not run: {error:#}");
                    None
                }
            };
            let _ = this.update(cx, |this, cx| {
                let Some(flow) = this.goto.as_mut() else { return };
                let i18n = &mut cx.default_global::<Localization>().0;
                let step = flow.fallback(targets, i18n);
                this.goto_step(step, GotoKind::Definition, pos, cx);
            });
        });
        self.keep(task);
    }

    /// Opens the chooser of several targets, with the lines of their files.
    fn choose(&mut self, chooser: Chooser, cx: &mut Context<Self>) {
        let paths: Vec<std::path::PathBuf> =
            chooser.targets.iter().map(|target| target.location.path.0.clone()).collect();
        let read = cx.background_executor().spawn(async move {
            let mut lines = HashMap::new();
            for path in paths {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    lines.insert(to_ipc(&path), text.lines().map(str::to_owned).collect::<Vec<_>>());
                }
            }
            lines
        });
        let root = cx
            .global::<Backend>()
            .host
            .project(&self.window_label)
            .map(|project| to_ipc(&project.root))
            .unwrap_or_default();
        let task = cx.spawn(async move |this, cx| {
            let lines = read.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(open) = this.chooser.as_ref().and_then(WeakEntity::upgrade) {
                    open.update(cx, |open, cx| open.set_chooser(chooser, &lines, &root, cx));
                    return;
                }
                let view = cx.new(|cx| TargetsChooser::new(chooser, &lines, &root, cx));
                this.chooser = Some(view.downgrade());
                cx.emit(IntelligenceEvent::Choose(view));
            });
        });
        self.keep(task);
    }

    /// The chooser closed: a search still running is cancelled.
    pub fn chooser_closed(&mut self, cx: &mut Context<Self>) {
        self.chooser = None;
        if let Some(id) = self.goto.as_mut().and_then(GotoFlow::chooser_closed) {
            cx.global::<Backend>().host.cancel_request(id);
        }
    }

    /// A press the pointer handler took: the caret goes where it was pressed, then the navigation
    /// runs.
    fn navigate_from_pointer(&mut self, navigation: PointerNavigation, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        let caret = match &navigation {
            PointerNavigation::Pass => return,
            PointerNavigation::Run { caret, .. } | PointerNavigation::Declaration { caret } => *caret,
        };
        editor.update(cx, |editor, cx| editor.select(caret, caret, cx));
        match navigation {
            PointerNavigation::Run { action, .. } => {
                window.defer(cx, move |window, cx| {
                    crate::platform::actions::execute_action_from(
                        crate::platform::actions::ActionSource::Other,
                        &action,
                        None,
                        window,
                        cx,
                    );
                });
            }
            PointerNavigation::Declaration { .. } => self.goto(GotoKind::Definition, cx),
            PointerNavigation::Pass => {}
        }
    }

    fn popup_position(&self, offset: u32, below: bool, cx: &App) -> Option<gpui_kit::Point<gpui_kit::Pixels>> {
        let editor = self.editor.upgrade()?;
        let editor = editor.read(cx);
        let top = editor.position_of(offset)?;
        Some(if below { point(top.x, top.y + editor.line_height(cx)) } else { top })
    }
}

impl EditorIntelligence {
    // -----------------------------------------------------------------------------------------
    // Problems

    /// A server published the problems of the document: they are underlined in the text.
    fn diagnostics_published(&mut self, source: &str, items: Vec<ide_core::Diagnostic>, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        if self.diagnostics.published(&self.path, source, items).is_none() {
            return;
        }
        let profile = ProfileOverrides::default();
        let styled = self.diagnostics.diagnostics_for(&self.path, &profile);
        self.lint = to_lint_diagnostics(editor.read(cx).rope(), &styled);
        let marks = self.lint.iter().filter_map(problem_mark).collect();
        editor.update(cx, |editor, cx| editor.set_marks(MarkLayer::Diagnostics, marks, cx));
        cx.notify();
    }

    /// The problems move with the text until the server publishes them again.
    fn follow_diagnostics(&mut self, changes: &[TextChange], editor: &Entity<Editor>, cx: &App) {
        let len = buffer::len_utf16(editor.read(cx).rope());
        for lint in &mut self.lint {
            lint.from = buffer::map_offset(lint.from, changes, buffer::Assoc::After).min(len);
            lint.to = buffer::map_offset(lint.to, changes, buffer::Assoc::Before).max(lint.from).min(len);
        }
    }

    /// The problems of the document, as shown.
    pub fn problems(&self) -> &[LintDiagnostic] {
        &self.lint
    }

    // -----------------------------------------------------------------------------------------
    // Hover and Quick Documentation

    /// The pointer moved over the text (`offset`) or left it: once it rests, the problems and the
    /// documentation there show above it. Leaving closes the hover, unless the pointer went into it.
    fn pointer_rests(&mut self, offset: Option<u32>, modifiers: gpui_kit::Modifiers, cx: &mut Context<Self>) {
        let held = hover::Modifiers {
            meta: modifiers.platform,
            ctrl: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
        };
        // The pointer went from the text into the hover popup, which stays.
        if offset.is_none() && self.pointer_in_hover {
            return;
        }
        let changed = self.navigation_modifier.pointer_event(held, PointerTarget::Other);
        if changed.is_some_and(|held| self.navigation_modifier.closes_hover(held, self.hover_range.is_some())) {
            self.close_hover(cx);
        }
        if let (Some(offset), Some((from, to))) = (offset, self.hover_range)
            && from <= offset
            && offset <= to
        {
            return;
        }
        self.hover_ticket += 1;
        let ticket = self.hover_ticket;
        let settings = crate::platform::settings::effective(&self.window_label, cx).editor.code_editing;
        let task = match offset {
            Some(offset) => {
                let delay = u64::from(settings.quick_doc_delay).max(100);
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(delay)).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.hover_ticket == ticket {
                            this.close_hover(cx);
                            this.ask_hover(offset, settings.quick_doc_on_hover, false, cx);
                        }
                    });
                })
            }
            None => cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(HOVER_LEAVE_MS)).await;
                let _ = this.update(cx, |this, cx| {
                    if this.hover_ticket == ticket && !this.pointer_in_hover {
                        this.close_hover(cx);
                    }
                });
            }),
        };
        self.keep(task);
    }

    /// Asks for the documentation at `offset` (with `documentation`), then shows it with the
    /// problems there. `explicit` is Quick Documentation, which says so when there is nothing.
    fn ask_hover(&mut self, offset: u32, documentation: bool, explicit: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        let problems: Vec<LintDiagnostic> =
            if explicit { Vec::new() } else { diagnostics_at(&self.lint, offset).into_iter().cloned().collect() };
        let ticket = self.hover_ticket;
        if !documentation {
            self.show_hover(offset, problems, None, cx);
            return;
        }
        if let Some(previous) = self.hover_request.take() {
            cx.global::<Backend>().host.cancel_request(previous);
        }
        let id = Backend::next_request_id();
        self.hover_request = Some(id);
        let pos = offset_to_pos(editor.read(cx).rope(), offset);
        let (window, doc) = (self.window_label.clone(), self.doc);
        let work = cx
            .global::<Backend>()
            .with_host(move |host| async move { ide_host::ops::lsp::lsp_hover(&host, &window, id, doc, pos).await });
        let task = cx.spawn(async move |this, cx| {
            let answer = work.await.unwrap_or_else(|error| Err(IdeError::internal(error.to_string())));
            let _ = this.update(cx, |this, cx| {
                if this.hover_request != Some(id) || this.hover_ticket != ticket {
                    return;
                }
                this.hover_request = None;
                let hover = match answer {
                    Ok(hover) => hover,
                    Err(error) => {
                        tracing::debug!("no hover: {}", error.message);
                        None
                    }
                };
                if explicit && hover.is_none() {
                    this.show_hint_text(NO_DOCUMENTATION, cx);
                    return;
                }
                this.show_hover(offset, problems, hover, cx);
            });
        });
        self.keep(task);
    }

    fn show_hover(
        &mut self,
        offset: u32,
        problems: Vec<LintDiagnostic>,
        hover: Option<ide_lsp::types::Hover>,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.editor.upgrade() else { return };
        let popup = hover_popup(&self.navigation_modifier, editor.read(cx).rope(), offset, problems, hover);
        let Some(popup) = popup else { return };
        self.hover_range = Some((popup.from, popup.to));
        self.hover.update(cx, |view, cx| view.show(&popup, cx));
        cx.notify();
    }

    /// The window's modifier keys changed with the pointer still: holding the navigation modifier
    /// closes the documentation the pointer is not in, as moving with it held does.
    pub fn modifiers_changed(&mut self, modifiers: gpui_kit::Modifiers, cx: &mut Context<Self>) {
        let held = hover::Modifiers {
            meta: modifiers.platform,
            ctrl: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
        };
        let changed = self.navigation_modifier.key(held);
        if changed.is_some_and(|held| self.navigation_modifier.closes_hover(held, self.hover_range.is_some())) {
            self.close_hover(cx);
        }
    }

    /// The window lost the keyboard: the modifier's release will not be seen, so it counts as up.
    pub fn window_deactivated(&mut self) {
        self.navigation_modifier.blur();
    }

    fn close_hover(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.hover_request.take() {
            cx.global::<Backend>().host.cancel_request(id);
        }
        if self.hover_range.take().is_some() {
            self.pointer_in_hover = false;
            self.hover.update(cx, |view, cx| view.clear(cx));
            cx.notify();
        }
    }

    /// The editor lost the keyboard: what shows over it closes.
    pub fn editor_blurred(&mut self, cx: &mut Context<Self>) {
        self.close_lookup(cx);
        self.close_hover(cx);
        if self.signature.close() {
            self.params.update(cx, |params, cx| params.show(None, cx));
        }
        if self.hints.hide() {
            self.show_hint(cx);
        }
        cx.notify();
    }

    /// Code › Quick Documentation: the documentation of the name at the caret, above it.
    pub fn quick_documentation(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        let caret = editor.read(cx).caret();
        self.close_hover(cx);
        self.hover_ticket += 1;
        self.ask_hover(caret, true, true, cx);
    }

    fn hover_event(&mut self, event: &HoverEvent, cx: &mut Context<Self>) {
        match event {
            &HoverEvent::MoreActions { at } => {
                self.close_hover(cx);
                if let Some(editor) = self.editor.upgrade() {
                    editor.update(cx, |editor, cx| editor.select(at, at, cx));
                }
                cx.emit(IntelligenceEvent::RunAction(SHOW_INTENTIONS_ACTION));
            }
            HoverEvent::QuickFix { .. } | HoverEvent::Link(_) => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Parameter info

    /// An edit or a caret move: parameter info opens after `(` or `,` (once the setting's delay
    /// passed), and follows the caret while it shows.
    fn signature_follow(&mut self, changes: &[TextChange], moved: bool, typed: Option<char>, cx: &mut Context<Self>) {
        let settings = self.settings(cx);
        let delay = settings.parameter_info.then_some(u64::from(settings.parameter_info_delay));
        let delay_ms = match self.signature.update(changes, moved, typed, delay) {
            SignatureFollowUp::None => return,
            SignatureFollowUp::AutoPopup(ms) => ms,
            SignatureFollowUp::Refresh => REFRESH_MS,
        };
        let version = self.editor.upgrade().map(|editor| editor.read(cx).version());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(delay_ms)).await;
            let _ = this.update(cx, |this, cx| {
                // A later edit asks for itself.
                if this.editor.upgrade().map(|editor| editor.read(cx).version()) == version {
                    this.ask_signature(false, cx);
                }
            });
        });
        self.keep(task);
    }

    /// Asks the server for the signature help at the caret; `explicit` is Code › Parameter Info.
    pub fn ask_signature(&mut self, explicit: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        let (rope, caret, version) = {
            let editor = editor.read(cx);
            (editor.rope().clone(), editor.caret(), editor.version())
        };
        let (window, doc, pos) = (self.window_label.clone(), self.doc, offset_to_pos(&rope, caret));
        let work = cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::lsp::lsp_signature_help(&host, &window, doc, pos).await
        });
        let task = cx.spawn(async move |this, cx| {
            let answer = work.await.unwrap_or_else(|error| Err(IdeError::internal(error.to_string())));
            let _ = this.update(cx, |this, cx| {
                let Some(editor) = this.editor.upgrade() else { return };
                let (now, head, len) = {
                    let editor = editor.read(cx);
                    (editor.version(), editor.caret(), buffer::len_utf16(editor.rope()))
                };
                let hint = match answer {
                    Ok(help) => this.signature.answer(help, now == version && head == caret, explicit, head, len),
                    Err(error) => {
                        tracing::debug!("no signature help: {}", error.message);
                        this.signature.failed();
                        explicit.then_some(NO_PARAMETERS)
                    }
                };
                if let Some(hint) = hint {
                    this.show_hint_text(hint, cx);
                }
                let shown = this.signature.shown().cloned();
                this.params.update(cx, |params, cx| params.show(shown.as_ref(), cx));
                cx.notify();
            });
        });
        self.keep(task);
    }

    /// A completed call left the caret inside its `()`: parameter info when the call takes
    /// arguments, else the caret moves past `)`.
    fn parameter_info_or_leave(&mut self, inside: u32, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        let (rope, version) = {
            let editor = editor.read(cx);
            (editor.rope().clone(), editor.version())
        };
        let (window, doc, pos) = (self.window_label.clone(), self.doc, offset_to_pos(&rope, inside));
        let work = cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::lsp::lsp_signature_help(&host, &window, doc, pos).await
        });
        let task = cx.spawn(async move |this, cx| {
            let help = match work.await {
                Ok(Ok(help)) => help,
                Ok(Err(error)) => {
                    tracing::debug!("no signature help: {}", error.message);
                    None
                }
                Err(error) => {
                    tracing::warn!("signature help did not run: {error:#}");
                    None
                }
            };
            let _ = this.update(cx, |this, cx| {
                let Some(editor) = this.editor.upgrade() else { return };
                let still_there =
                    editor.read_with(cx, |editor, _| editor.version() == version && editor.caret() == inside);
                if let Some(caret) = this.signature.parameter_info_or_leave(help, still_there, inside) {
                    editor.update(cx, |editor, cx| editor.select(caret, caret, cx));
                }
                let shown = this.signature.shown().cloned();
                this.params.update(cx, |params, cx| params.show(shown.as_ref(), cx));
                cx.notify();
            });
        });
        self.keep(task);
    }
}

impl EditorIntelligence {
    // -----------------------------------------------------------------------------------------
    // Intentions

    /// Code › Show Context Actions: the server's code actions for the selection, quick fixes of the
    /// problems there first; "No intentions available" when there are none.
    pub fn show_intentions(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        self.close_lookup(cx);
        self.close_hover(cx);
        let range = editor.read(cx).selection_range();
        let (window, doc) = (self.window_label.clone(), self.doc);
        let work = cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::lsp::lsp_code_actions(&host, &window, doc, range, Vec::new()).await
        });
        let version = editor.read(cx).version();
        let task = cx.spawn(async move |this, cx| {
            let actions = match work.await {
                Ok(Ok(actions)) => actions,
                Ok(Err(error)) => {
                    tracing::debug!("no code actions: {}", error.message);
                    Vec::new()
                }
                Err(error) => {
                    tracing::warn!("code actions did not run: {error:#}");
                    Vec::new()
                }
            };
            let _ = this.update(cx, |this, cx| this.list_intentions(actions, version, cx));
        });
        self.keep(task);
    }

    fn list_intentions(&mut self, actions: Vec<CodeAction>, version: u32, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else { return };
        let (rope, caret, now) = {
            let editor = editor.read(cx);
            (editor.rope().clone(), editor.caret(), editor.version())
        };
        if now != version {
            return;
        }
        let actions = listed_code_actions(&actions);
        let line = buffer::line_range(&rope, buffer::line_col(&rope, caret).0);
        let fix_icon = quick_fix_icon(&self.lint, line.start, line.end);
        let items = listed_intentions(action_items(&actions, fix_icon), fix_icon);
        let inspections = self.diagnostics.inspections_at(&self.path, &rope, caret, &ProfileOverrides::default());
        let items = with_inspection_controls(items, &inspections, &mut cx.default_global::<Localization>().0);
        if items.is_empty() {
            self.show_hint_text(NO_INTENTIONS, cx);
            return;
        }
        let list = cx.new(|cx| intentions_popup(&items, cx));
        let events = cx.subscribe(&list, |this, list, event: &ListPopupEvent, cx| match event {
            ListPopupEvent::PreviewWanted(id) => this.preview_intention(&list, id, cx),
            ListPopupEvent::Activated(id) => this.run_intention(id, cx),
            ListPopupEvent::Dismissed | ListPopupEvent::Pin | ListPopupEvent::Control { .. } => {
                this.intentions = None;
            }
            ListPopupEvent::Active(_) => {}
        });
        let resolved = ResolveCache::default();
        self.intentions = Some(OpenIntentions { list: list.downgrade(), items, actions, resolved, _events: events });
        cx.emit(IntelligenceEvent::Intentions(list));
    }

    /// The intentions list while it is open.
    pub fn intentions(&self) -> Option<Entity<ListPopup>> {
        self.intentions.as_ref().and_then(|open| open.list.upgrade())
    }

    fn code_action_of(&self, id: &str) -> Option<usize> {
        let open = self.intentions.as_ref()?;
        match open.items.iter().find(|item| item.id == id)?.run {
            IntentionRun::CodeAction(index) => Some(index),
            IntentionRun::Local(_) | IntentionRun::Inspection(_) => None,
        }
    }

    /// The selection rested on row `id`: its preview, resolved once from the server when the action
    /// computes its edit lazily.
    fn preview_intention(&mut self, list: &Entity<ListPopup>, id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.code_action_of(id) else { return };
        let Some(editor) = self.editor.upgrade() else { return };
        let rope = editor.read(cx).rope().clone();
        let path = self.path.clone();
        let Some(open) = self.intentions.as_mut() else { return };
        let Some(action) = open.actions.get(index).cloned() else { return };
        match open.resolved.preview(&index, &action, &path, &rope) {
            PreviewStep::Show(preview) => list.update(cx, |list, cx| list.set_preview(id, Some(preview), cx)),
            PreviewStep::Waiting => {}
            PreviewStep::Resolve => {
                let (window, doc, list, id) = (self.window_label.clone(), self.doc, list.downgrade(), id.to_owned());
                let work = cx.global::<Backend>().with_host(move |host| async move {
                    ide_host::ops::lsp::lsp_resolve_code_action(&host, &window, doc, action).await
                });
                let task = cx.spawn(async move |this, cx| {
                    let answer = work.await.unwrap_or_else(|error| Err(IdeError::internal(error.to_string())));
                    let _ = this.update(cx, |this, cx| {
                        let Some(open) = this.intentions.as_mut() else { return };
                        open.resolved.resolved(&index, answer);
                        if let Some(list) = list.upgrade() {
                            this.preview_intention(&list, &id, cx);
                        }
                    });
                });
                self.keep(task);
            }
        }
    }

    /// Row `id` was chosen: its code action (resolved, when the preview resolved it) is applied by
    /// the backend, whose edit the editor then follows.
    fn run_intention(&mut self, id: &str, cx: &mut Context<Self>) {
        let index = self.code_action_of(id);
        let Some(open) = self.intentions.take() else { return };
        let Some(action) = index.and_then(|index| open.actions.get(index).map(|listed| (index, listed))) else {
            tracing::debug!("intention {id} is not a code action");
            return;
        };
        let action = open.resolved.action_to_apply(&action.0, action.1).clone();
        let (window, doc) = (self.window_label.clone(), self.doc);
        let work = cx.global::<Backend>().with_host(move |host| async move {
            ide_host::ops::lsp::lsp_apply_code_action(&host, &window, doc, action).await
        });
        let task = cx.spawn(async move |this, cx| {
            let failure = match work.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.message,
                Err(error) => error.to_string(),
            };
            tracing::warn!("the code action failed: {failure}");
            let _ = this.update(cx, |this, cx| this.show_hint_text(&failure, cx));
        });
        self.keep(task);
    }

    /// The intentions popup closed without a choice.
    pub fn intentions_closed(&mut self) {
        self.intentions = None;
    }
}

/// How long the hover waits, once the pointer left the text, for it to come into the popup.
const HOVER_LEAVE_MS: u64 = 300;
/// How long typing pauses before it starts completion.
const ACTIVATE_ON_TYPING_DELAY_MS: u64 = 100;
const NO_DOCUMENTATION: &str = "No documentation found";
const NO_INTENTIONS: &str = "No intentions available";

/// An open intentions list.
struct OpenIntentions {
    list: WeakEntity<ListPopup>,
    items: Vec<IntentionItem>,
    actions: Vec<CodeAction>,
    resolved: ResolveCache<usize>,
    _events: Subscription,
}
const SHOW_INTENTIONS_ACTION: &str = "code.showIntentions";

/// The underline of a problem; hints have none.
fn problem_mark(lint: &LintDiagnostic) -> Option<Mark> {
    let kind = match lint.severity {
        Severity::Error => MarkKind::ErrorProblem,
        Severity::Warning => MarkKind::WarningProblem,
        Severity::Info => MarkKind::WeakProblem,
        Severity::Hint => return None,
    };
    Some(Mark { range: lint.from..lint.to, kind })
}

/// Runs `act` on the intelligence of the editor of the window an action was invoked in.
fn on_intelligence(
    window: &mut Window,
    cx: &mut App,
    act: impl FnOnce(&mut EditorIntelligence, &mut Context<EditorIntelligence>),
) -> anyhow::Result<()> {
    let editor = crate::workbench::active_editor(window, cx)
        .ok_or_else(|| anyhow::anyhow!("no editor is open in this window"))?;
    let intelligence =
        EditorIntelligence::of(&editor, cx).ok_or_else(|| anyhow::anyhow!("the editor has no code intelligence"))?;
    intelligence.update(cx, act);
    Ok(())
}

/// The code intelligence's entries in the action registry.
pub fn register(cx: &mut App) {
    let registry = cx.default_global::<crate::platform::actions::ActionRegistry>();
    registry.handle("code.completion", |_, window, cx| on_intelligence(window, cx, |this, cx| this.complete(true, cx)));
    registry.handle("code.quickDocumentation", |_, window, cx| {
        on_intelligence(window, cx, |this, cx| this.quick_documentation(cx))
    });
    registry.handle("code.showIntentions", |_, window, cx| {
        on_intelligence(window, cx, |this, cx| this.show_intentions(cx))
    });
    registry.handle("code.parameterInfo", |_, window, cx| {
        on_intelligence(window, cx, |this, cx| this.ask_signature(true, cx))
    });
    for (id, kind) in [
        ("navigate.declaration", GotoKind::Definition),
        ("navigate.implementation", GotoKind::Implementation),
        ("navigate.typeDeclaration", GotoKind::TypeDefinition),
    ] {
        registry.handle(id, move |_, window, cx| on_intelligence(window, cx, |this, cx| this.goto(kind, cx)));
    }
}

/// What a press over the editor does first: a navigation gesture (⌘-click, a mouse shortcut of the
/// keymap) is taken and carried out once the editor's press handling is over.
fn pointer_handler(intelligence: WeakEntity<EditorIntelligence>) -> PointerHandler {
    Rc::new(move |event: &MouseDownEvent, offset: Option<u32>, window: &mut Window, cx: &mut App| {
        if event.click_count > 1 {
            return false;
        }
        let button = input::button_index(event.button);
        let mods = input::mods(&event.modifiers);
        let mac = crate::platform::keymap::is_mac_host();
        let navigation = input::with_dispatcher(window, cx, |dispatcher, env| {
            pointer_navigation(button, mods, offset, mac, dispatcher, env)
        });
        if navigation == PointerNavigation::Pass {
            return false;
        }
        let intelligence = intelligence.clone();
        window.defer(cx, move |window, cx| {
            let _ = intelligence.update(cx, |this, cx| this.navigate_from_pointer(navigation, window, cx));
        });
        true
    })
}

fn match_case(settings: &ide_core::editor_settings::CompletionSettings) -> MatchCase {
    match (settings.match_case, settings.case_sensitivity.as_str()) {
        (false, _) => MatchCase::Off,
        (true, "all") => MatchCase::AllLetters,
        (true, _) => MatchCase::FirstLetter,
    }
}

/// The local time the snippet variables read.
fn clock() -> SnippetClock {
    use chrono::{Datelike, Timelike};
    let now = chrono::Local::now();
    SnippetClock {
        year: now.year(),
        month: now.month(),
        day: now.day(),
        hour: now.hour(),
        minute: now.minute(),
        second: now.second(),
    }
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

fn shifted(offset: u32, by: i64) -> u32 {
    (i64::from(offset) + by).max(0) as u32
}

impl Render for EditorIntelligence {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lookup = self.lookup.read(cx).is_open().then(|| self.popup_position(self.lookup_from, true, cx)).flatten();
        let hint = self.hints.hint().and_then(|hint| self.popup_position(hint.pos, false, cx));
        let params = self.signature.shown().and_then(|shown| self.popup_position(shown.pos, false, cx));
        let hover = self.hover_range.and_then(|(from, _)| self.popup_position(from, false, cx));
        // Above the line: the popup's bottom edge on the line's top.
        let above = |at, child: gpui_kit::AnyElement| {
            deferred(anchored().anchor(Anchor::BottomLeft).position(at).snap_to_window().child(child)).with_priority(1)
        };
        let hover = hover.map(|at| {
            let shown = div()
                .id("editor-hover")
                .occlude()
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    this.pointer_in_hover = *hovered;
                    if !hovered {
                        this.pointer_rests(None, gpui_kit::Modifiers::default(), cx);
                    }
                }))
                .child(self.hover.clone());
            above(at, shown.into_any_element())
        });
        div()
            .children(lookup.map(|at| {
                let lookup = div().occlude().child(self.lookup.clone());
                deferred(anchored().position(at).snap_to_window().child(lookup)).with_priority(1)
            }))
            .children(params.map(|at| above(at, div().occlude().child(self.params.clone()).into_any_element())))
            .children(hover)
            .children(hint.map(|at| above(at, self.hint_view.clone().into_any_element())))
            .text_size(theme::length(cx, "fs-ui"))
    }
}

#[cfg(test)]
pub(crate) mod fake_server {
    //! A language server for tests: a Python script speaking LSP over stdio that answers each
    //! method from a JSON file the test writes (and may rewrite between steps), and logs every
    //! message it receives.

    use std::path::{Path, PathBuf};

    const SERVER: &str = r#"import sys, json, time
script_path, log_path = sys.argv[1], sys.argv[2]
def send(v):
    body = json.dumps(v).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
def script():
    try:
        with open(script_path) as f:
            return json.load(f)
    except Exception:
        return {}
def log(msg):
    with open(log_path, 'a') as f:
        f.write(json.dumps(msg) + '\n')
def publish(uri):
    diagnostics = script().get('diagnostics')
    if diagnostics is not None:
        send({'jsonrpc': '2.0', 'method': 'textDocument/publishDiagnostics',
              'params': {'uri': uri, 'diagnostics': diagnostics}})
counts = {}
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b'\r\n', b'\n'):
            break
        k, v = line.decode().split(':', 1)
        headers[k.lower()] = v.strip()
    msg = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    log(msg)
    method = msg.get('method')
    params = msg.get('params', {})
    if method == 'initialize':
        result = {'capabilities': {
            'textDocumentSync': 1,
            'completionProvider': {'triggerCharacters': ['.'], 'resolveProvider': True},
            'hoverProvider': True,
            'signatureHelpProvider': {'triggerCharacters': ['(', ','], 'retriggerCharacters': [',']},
            'definitionProvider': True,
            'declarationProvider': True,
            'implementationProvider': True,
            'typeDefinitionProvider': True,
            'codeActionProvider': {'resolveProvider': True},
            **script().get('capabilities', {}),
        }}
    elif method in ('textDocument/didOpen', 'textDocument/didChange'):
        publish(params['textDocument']['uri'])
        continue
    elif method == 'shutdown':
        result = None
    elif method == 'exit':
        sys.exit(0)
    elif 'id' not in msg:
        continue
    else:
        answers = script()
        n = counts.get(method, 0)
        counts[method] = n + 1
        delays = answers.get('delays', {}).get(method, [])
        if n < len(delays) and delays[n]:
            time.sleep(delays[n])
        result = answers.get(method)
        if isinstance(result, dict) and 'by_call' in result:
            calls = result['by_call']
            result = calls[min(n, len(calls) - 1)]
        if isinstance(result, dict) and 'by_column' in result:
            column = str(params.get('position', {}).get('character'))
            delay = result.get('delay', {}).get(column)
            if delay:
                time.sleep(delay)
            result = result['by_column'].get(column)
    send({'jsonrpc': '2.0', 'id': msg['id'], 'result': result})
"#;

    /// Where a test's fake server lives: its script, its answers and its log.
    pub(crate) struct FakeServer {
        pub answers: PathBuf,
        pub log: PathBuf,
    }

    impl FakeServer {
        /// Installs, under `config` (the app's configuration directory, before the backend starts),
        /// the language `mock` of `*.mock` files whose server is the fake one.
        pub(crate) fn install(config: &Path, scratch: &Path) -> Self {
            let script = scratch.join("server.py");
            let answers = scratch.join("answers.json");
            let log = scratch.join("log.jsonl");
            std::fs::write(&script, SERVER).expect("the server script is written");
            std::fs::write(&answers, "{}").expect("the answers are written");
            let languages = config.join("languages");
            std::fs::create_dir_all(&languages).expect("the languages folder is made");
            let toml = format!(
                "id = \"mock\"\nname = \"Mock\"\nextensions = [\"mock\"]\nline-comment = \"//\"\n\n\
                 [[language-servers]]\nname = \"mock-ls\"\ncommand = \"python3\"\nargs = [{:?}, {:?}, {:?}]\n",
                script.to_string_lossy(),
                answers.to_string_lossy(),
                log.to_string_lossy(),
            );
            std::fs::write(languages.join("mock.toml"), toml).expect("the language is written");
            Self { answers, log }
        }

        /// Makes the server answer `answers`: method → result, or `{"by_call": [result, …]}` (one per
        /// call, the last repeating), or `{"by_column": {column: result}, "delay": {column: seconds}}`
        /// (by the request's position); `delays`: method → seconds per call; `diagnostics`: published
        /// on every change; `capabilities`: added to the server's own when it starts.
        pub(crate) fn answer(&self, answers: serde_json::Value) {
            std::fs::write(&self.answers, answers.to_string()).expect("the answers are written");
        }

        /// The methods of the messages the server received, in order.
        pub(crate) fn received(&self) -> Vec<serde_json::Value> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake_server::FakeServer;
    use super::*;
    use crate::backend::AppDirs;
    use crate::platform::actions::execute_action_or_throw;
    use crate::platform::storage::UiStorage;
    use crate::workbench::{self, Workbench};
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext, px};
    use serde_json::{Value, json};
    use std::path::PathBuf;

    mod proofs9_cancel_task;
    mod proofs9_resolve;
    mod proofs9_syntax_usages;
    mod proofs_editor_lsp;
    mod proofs_editor_navigation_modifier;

    /// A trusted project of `files` in a window, in Full mode with the fake server for `*.mock`
    /// files answering `answers`, and `open` open in the editor with the keyboard.
    struct Fixture {
        workbench: Entity<Workbench>,
        root: PathBuf,
        server: FakeServer,
        _folders: (tempfile::TempDir, tempfile::TempDir),
    }

    fn open<'a>(
        cx: &'a mut TestAppContext,
        files: &[(&str, &str)],
        open: &str,
        answers: Value,
    ) -> (Fixture, &'a mut VisualTestContext) {
        open_decided(cx, files, open, answers, true)
    }

    /// [`open`], with the project trusted or previewed in Restricted Mode as `trusted` says.
    fn open_decided<'a>(
        cx: &'a mut TestAppContext,
        files: &[(&str, &str)],
        open: &str,
        answers: Value,
        trusted: bool,
    ) -> (Fixture, &'a mut VisualTestContext) {
        cx.executor().allow_parking();
        let (home, folder) = (tempfile::tempdir().expect("a home"), tempfile::tempdir().expect("a folder"));
        let dirs = AppDirs::under(home.path());
        let server = FakeServer::install(&dirs.config, home.path());
        server.answer(answers);
        let root = folder.path().canonicalize().expect("the folder exists");
        for (path, text) in files {
            std::fs::write(root.join(path), text).expect("the file is written");
        }
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            input::init(cx);
            crate::editor::actions::register(cx);
            workbench::register_actions(cx);
            cx.set_global(UiStorage::default());
            Backend::init(dirs, cx).expect("the backend starts");
            workbench::menu::init(cx);
            let host = cx.global::<Backend>().host.clone();
            host.global_settings
                .set(ide_workspace::settings::SettingsScope::Global, "ide.mode", json!("full"))
                .expect("Full mode is set");
            host.trust.decide(&root, trusted, false).expect("the project's trust is decided");
        });
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let file = root.join(open);
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project_file(file, window, cx));
        let fixture = Fixture { workbench, root, server, _folders: (home, folder) };
        settle(cx, |cx| {
            let editor = fixture.editor(cx);
            cx.update(|window, cx| editor.is_some_and(|editor| editor.focus_handle(cx).is_focused(window)))
        });
        (fixture, cx)
    }

    // Not from a spec: a project in Restricted Mode starts no language server. Its file opens and the
    // server hears nothing, not even `initialize`; once the project is trusted the server gets it.
    #[gpui_kit::test]
    async fn a_project_in_restricted_mode_starts_no_language_server_until_it_is_trusted(cx: &mut TestAppContext) {
        let (fixture, cx) = open_decided(cx, &[("a.mock", "one\n")], "a.mock", json!({}), false);
        assert!(fixture.workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        for _ in 0..200 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(fixture.server.received().is_empty(), "{:?}", fixture.server.received());

        fixture.run(cx, "project.trust");
        settle(cx, |cx| !fixture.workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        settle(cx, |_| !fixture.requests("textDocument/didOpen").is_empty());
        assert_eq!(fixture.requests("initialize").len(), 1);
        fixture.close(cx);
    }

    fn settle(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the backend did not deliver within four seconds");
    }

    impl Fixture {
        fn editor(&self, cx: &mut VisualTestContext) -> Option<Entity<Editor>> {
            self.workbench.read_with(cx, |workbench, _| workbench.editor().cloned())
        }

        fn intelligence(&self, cx: &mut VisualTestContext) -> Entity<EditorIntelligence> {
            let editor = self.editor(cx).expect("an editor");
            cx.update(|_, cx| EditorIntelligence::of(&editor, cx)).expect("the editor's intelligence")
        }

        fn text(&self, cx: &mut VisualTestContext) -> String {
            self.editor(cx).expect("an editor").read_with(cx, |editor, _| editor.text())
        }

        fn rows(&self, cx: &mut VisualTestContext) -> Vec<String> {
            let lookup = self.intelligence(cx).read_with(cx, |this, _| this.lookup().clone());
            lookup.read_with(cx, |lookup, _| lookup.rows())
        }

        fn requests(&self, method: &str) -> Vec<Value> {
            self.server.received().into_iter().filter(|message| message["method"] == method).collect()
        }

        fn put_caret(&self, cx: &mut VisualTestContext, offset: u32) {
            let editor = self.editor(cx).expect("an editor");
            editor.update(cx, |editor, cx| editor.select(offset, offset, cx));
            cx.run_until_parked();
        }

        fn run(&self, cx: &mut VisualTestContext, id: &str) {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx))
                .unwrap_or_else(|error| panic!("{id}: {error}"));
            cx.run_until_parked();
        }

        fn close(&self, cx: &mut VisualTestContext) {
            cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
        }
    }

    fn item(label: &str, kind: u32) -> Value {
        json!({ "label": label, "kind": kind, "insertText": label })
    }

    /// Typing `obj.` asks the server (trigger character `.`) and lists its items; typing narrows
    /// them; Enter puts the item and its additional edit in as one undo step; Code Completion ›
    /// Basic opens it again and Escape closes it, leaving the text; nothing found shows "No
    /// suggestions".
    #[gpui_kit::test]
    async fn typing_completes_from_the_server_and_enter_inserts_the_item_with_its_import(cx: &mut TestAppContext) {
        let mut with_import = item("fooBar", 6);
        let start = json!({ "line": 0, "character": 0 });
        let import = json!({ "range": { "start": start, "end": start }, "newText": "use foo;\n" });
        with_import["additionalTextEdits"] = json!([import]);
        let answers = json!({ "textDocument/completion": [with_import, item("fooBaz", 5), item("other", 6)] });
        let (f, cx) = open(cx, &[("a.mock", "")], "a.mock", answers);

        cx.simulate_input("obj.");
        settle(cx, |cx| !f.rows(cx).is_empty());
        assert_eq!(f.rows(cx), ["fooBar", "fooBaz", "other"]);
        let asked = f.requests("textDocument/completion");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["params"]["context"], json!({ "triggerKind": 2, "triggerCharacter": "." }));
        assert_eq!(asked[0]["params"]["position"], json!({ "line": 0, "character": 4 }));

        // Typing narrows the same rows without asking again.
        cx.simulate_input("foo");
        cx.run_until_parked();
        assert_eq!(f.rows(cx), ["fooBar", "fooBaz"]);
        let lookup = f.intelligence(cx).read_with(cx, |this, _| this.lookup().clone());
        assert_eq!(lookup.read_with(cx, |lookup, _| lookup.marked()), [vec!["foo"], vec!["foo"]]);
        assert_eq!(f.requests("textDocument/completion").len(), 1);

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(f.text(cx), "use foo;\nobj.fooBar");
        assert!(f.rows(cx).is_empty(), "the lookup closed");
        let caret = f.editor(cx).expect("an editor").read_with(cx, |editor, _| editor.caret());
        assert_eq!(caret, "use foo;\nobj.fooBar".encode_utf16().count() as u32);
        // One undo step takes the item and its import out.
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(f.text(cx), "obj.foo");

        // Basic completion opens it again; Escape closes it and the text stays.
        f.run(cx, "code.completion");
        settle(cx, |cx| !f.rows(cx).is_empty());
        assert_eq!(f.rows(cx), ["fooBar", "fooBaz"]);
        cx.simulate_keystrokes("escape");
        assert!(f.rows(cx).is_empty());
        assert_eq!(f.text(cx), "obj.foo");

        // Explicit completion that finds nothing: the hint says so.
        f.server.answer(json!({ "textDocument/completion": [] }));
        f.run(cx, "code.completion");
        settle(cx, |cx| f.intelligence(cx).read_with(cx, |this, _| this.hint().is_some()));
        assert_eq!(
            f.intelligence(cx).read_with(cx, |this, _| this.hint().map(str::to_owned)).as_deref(),
            Some("No suggestions")
        );
        assert!(f.rows(cx).is_empty());
        f.close(cx);
    }

    /// With the server already attached, a word typed in one go asks once, when the typing pauses,
    /// at its end: no request goes out for the characters before it.
    #[gpui_kit::test]
    async fn a_word_typed_in_one_go_asks_the_attached_server_once_at_its_end(cx: &mut TestAppContext) {
        let answers = json!({ "textDocument/completion": [item("fooBar", 6)] });
        let (f, cx) = open(cx, &[("a.mock", "")], "a.mock", answers);
        settle(cx, |_| !f.requests("textDocument/didOpen").is_empty());

        cx.simulate_input("obj.");
        cx.run_until_parked();
        assert!(f.requests("textDocument/completion").is_empty(), "nothing is asked while typing goes on");
        settle(cx, |cx| !f.rows(cx).is_empty());
        assert_eq!(f.rows(cx), ["fooBar"]);
        let asked = f.requests("textDocument/completion");
        assert_eq!(asked.len(), 1, "{asked:#?}");
        assert_eq!(asked[0]["params"]["context"], json!({ "triggerKind": 2, "triggerCharacter": "." }));
        assert_eq!(asked[0]["params"]["position"], json!({ "line": 0, "character": 4 }));

        // Moving the caret before the pause drops the pending start.
        cx.simulate_keystrokes("escape");
        cx.simulate_input(" ab");
        f.put_caret(cx, 0);
        settle(cx, |_| true);
        for _ in 0..4 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
        }
        assert_eq!(f.requests("textDocument/completion").len(), 1);
        assert!(f.rows(cx).is_empty());
        f.close(cx);
    }

    /// An answer that comes after a newer request went out is dropped: only the newer one's items
    /// show, though the server got the older one first.
    #[gpui_kit::test]
    async fn an_answer_overtaken_by_a_newer_request_is_dropped(cx: &mut TestAppContext) {
        let answers = json!({
            "textDocument/completion": {
                "by_column": { "4": [item("xStale", 6)], "5": [item("xFresh", 6)] },
                "delay": { "4": 0.4 },
            },
        });
        let (f, cx) = open(cx, &[("a.mock", "")], "a.mock", answers);
        cx.simulate_input("obj.");
        // The server is answering the first request when the next character asks again.
        settle(cx, |_| f.requests("textDocument/completion").len() == 1);
        cx.simulate_input("x");
        settle(cx, |cx| !f.rows(cx).is_empty());
        assert_eq!(f.rows(cx), ["xFresh"]);
        settle(cx, |_| f.requests("textDocument/completion").len() == 2);
        for _ in 0..30 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(f.rows(cx), ["xFresh"]);
        f.close(cx);
    }

    fn location(path: &std::path::Path, line: u32, col: u32) -> Value {
        let at = json!({ "line": line, "character": col });
        json!({ "uri": format!("file://{}", path.display()), "range": { "start": at, "end": at } })
    }

    /// Go to Declaration by ⌘-click: one target opens its file at the target; two show the chooser,
    /// whose Enter opens the selected one; none shows the hint.
    #[gpui_kit::test]
    async fn cmd_click_goes_to_the_declaration_or_chooses_between_several(cx: &mut TestAppContext) {
        let files = [("a.mock", "call target here\n"), ("b.mock", "zero\none\n  target\n"), ("c.mock", "x target\n")];
        let (f, cx) = open(cx, &files, "a.mock", json!({}));
        let (b, c) = (f.root.join("b.mock"), f.root.join("c.mock"));
        f.server.answer(json!({ "textDocument/definition": [location(&b, 2, 2)] }));
        let editor = f.editor(cx).expect("an editor");
        let at = editor.read_with(cx, |editor, cx| {
            let at = editor.position_of(7).expect("painted");
            gpui_kit::point(at.x + px(1.0), at.y + editor.line_height(cx) / 2.0)
        });
        let modifiers = cx.update(|_, cx| Modifiers::secondary_key(cx));
        cx.simulate_click(at, modifiers);
        settle(cx, |cx| f.editor(cx).is_some_and(|editor| editor.read_with(cx, |e, _| e.path() == b.as_path())));
        let opened = f.editor(cx).expect("the target's editor");
        settle(cx, |cx| opened.read_with(cx, |editor, _| editor.caret_position()) == (2, 2));
        let asked = f.requests("textDocument/definition");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["params"]["position"], json!({ "line": 0, "character": 7 }));
        // The click put the caret on the name before navigating.
        assert_eq!(editor.read_with(cx, |editor, _| editor.caret()), 7);

        // Two targets: the chooser lists them, Enter opens the selected one.
        f.workbench
            .update_in(cx, |workbench, window, cx| workbench.open_project_file(f.root.join("a.mock"), window, cx));
        settle(cx, |cx| f.editor(cx).is_some_and(|e| e == editor));
        f.server.answer(json!({ "textDocument/definition": [location(&b, 2, 2), location(&c, 0, 2)] }));
        f.put_caret(cx, 7);
        f.run(cx, "navigate.declaration");
        let intelligence = f.intelligence(cx);
        settle(cx, |cx| intelligence.read_with(cx, |this, _| this.chooser().is_some()));
        let chooser = intelligence.read_with(cx, |this, _| this.chooser()).expect("the chooser");
        let labels: Vec<String> =
            chooser.read_with(cx, |chooser, _| chooser.rows().iter().map(|row| row.detail.clone()).collect());
        assert_eq!(labels, ["b.mock · b.mock:3", "c.mock · c.mock:1"]);
        assert!(
            cx.update(|window, cx| chooser.focus_handle(cx).contains_focused(window, cx)),
            "the chooser has the keyboard"
        );
        cx.simulate_keystrokes("down enter");
        settle(cx, |cx| f.editor(cx).is_some_and(|editor| editor.read_with(cx, |e, _| e.path() == c.as_path())));
        assert!(intelligence.read_with(cx, |this, _| this.chooser().is_none()));

        // None: the hint at the caret.
        f.workbench
            .update_in(cx, |workbench, window, cx| workbench.open_project_file(f.root.join("a.mock"), window, cx));
        settle(cx, |cx| f.editor(cx).is_some_and(|e| e == editor));
        f.server.answer(json!({ "textDocument/definition": [] }));
        f.run(cx, "navigate.declaration");
        let not_found = |cx: &mut VisualTestContext| {
            intelligence.read_with(cx, |this, _| this.hint() == Some("Cannot find declaration to go to"))
        };
        settle(cx, not_found);

        // Go to Implementation and Go to Type Declaration ask their own methods.
        f.server.answer(json!({ "textDocument/implementation": [], "textDocument/typeDefinition": [] }));
        f.run(cx, "navigate.implementation");
        settle(cx, |cx| intelligence.read_with(cx, |this, _| this.hint() == Some("No implementations found")));
        f.run(cx, "navigate.typeDeclaration");
        settle(cx, |cx| intelligence.read_with(cx, |this, _| this.hint() == Some("Cannot find type declaration")));
        assert_eq!(f.requests("textDocument/implementation").len(), 1);
        assert_eq!(f.requests("textDocument/typeDefinition").len(), 1);
        f.close(cx);
    }

    fn range(line: u32, from: u32, to: u32) -> Value {
        json!({ "start": { "line": line, "character": from }, "end": { "line": line, "character": to } })
    }

    /// The server's problems are underlined in the text and follow edits; the pointer resting on
    /// a name shows its problem and its documentation above it, and leaving the text closes them;
    /// Quick Documentation shows the documentation at the caret.
    #[gpui_kit::test]
    async fn problems_are_underlined_and_the_hover_shows_them_with_the_documentation(cx: &mut TestAppContext) {
        let answers = json!({
            "diagnostics": [{ "range": range(0, 4, 7), "message": "unknown name", "severity": 1, "source": "mock" }],
            "textDocument/hover": { "contents": { "kind": "markdown", "value": "**bar** is a name" } },
        });
        let (f, cx) = open(cx, &[("a.mock", "foo bar baz\n")], "a.mock", answers);
        let editor = f.editor(cx).expect("an editor");
        let marks = |cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, _| editor.marks(MarkLayer::Diagnostics).to_vec())
        };
        settle(cx, |cx| !marks(cx).is_empty());
        assert_eq!(marks(cx), [Mark { range: 4..7, kind: MarkKind::ErrorProblem }]);

        let intelligence = f.intelligence(cx);
        let hover_lines = |cx: &mut VisualTestContext| {
            intelligence.read_with(cx, |this, _| this.hover().clone()).read_with(cx, |hover, _| hover.lines())
        };
        let (at, line_height) =
            editor.read_with(cx, |editor, cx| (editor.position_of(5).expect("painted"), editor.line_height(cx)));
        let over = gpui_kit::point(at.x + px(1.0), at.y + line_height / 2.0);
        cx.simulate_mouse_move(over, None, Modifiers::default());
        settle(cx, |cx| !hover_lines(cx).is_empty());
        assert_eq!(hover_lines(cx), ["error: unknown name", "  mock", "  More actions…", "bar is a name"]);
        let asked = f.requests("textDocument/hover");
        assert_eq!(
            asked.last().map(|hover| hover["params"]["position"].clone()),
            Some(json!({ "line": 0, "character": 5 }))
        );
        // The pointer going into the popup keeps it.
        let popup = cx.debug_bounds("hover-popup").expect("the hover is drawn");
        cx.simulate_mouse_move(popup.center(), None, Modifiers::default());
        for _ in 0..12 {
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
        }
        assert_eq!(hover_lines(cx), ["error: unknown name", "  mock", "  More actions…", "bar is a name"]);
        cx.simulate_mouse_move(over, None, Modifiers::default());
        // Beside the text, the hover goes away.
        cx.simulate_mouse_move(gpui_kit::point(over.x + px(3000.0), over.y), None, Modifiers::default());
        settle(cx, |cx| hover_lines(cx).is_empty());

        // An edit before the problem moves its underline with the text, until the server publishes
        // again (it publishes nothing from now on).
        f.server.answer(
            json!({ "textDocument/hover": { "contents": { "kind": "markdown", "value": "**bar** is a name" } } }),
        );
        f.put_caret(cx, 0);
        cx.simulate_input("x");
        assert_eq!(marks(cx), [Mark { range: 5..8, kind: MarkKind::ErrorProblem }]);

        // Quick Documentation at the caret.
        f.put_caret(cx, 2);
        f.run(cx, "code.quickDocumentation");
        settle(cx, |cx| !hover_lines(cx).is_empty());
        assert_eq!(hover_lines(cx), ["bar is a name"]);
        cx.simulate_keystrokes("escape");
        assert!(hover_lines(cx).is_empty());
        f.close(cx);
    }

    /// `(` opens parameter info once the delay passed, with the first parameter active; `,` moves
    /// it to the next; Code › Parameter Info asks at once; Escape closes it.
    #[gpui_kit::test]
    async fn parameter_info_opens_after_a_parenthesis_and_follows_the_commas(cx: &mut TestAppContext) {
        let help = |active: u32| {
            json!({
                "signatures": [{ "label": "call(a, b)", "parameters": [{ "label": [5, 6] }, { "label": [8, 9] }] }],
                "activeSignature": 0,
                "activeParameter": active,
            })
        };
        let answers = json!({ "textDocument/signatureHelp": { "by_column": { "5": help(0), "7": help(1) } } });
        let (f, cx) = open(cx, &[("a.mock", "")], "a.mock", answers);
        let params = f.intelligence(cx).read_with(cx, |this, _| this.parameter_info().clone());
        let rows = |cx: &mut VisualTestContext| params.read_with(cx, |params, _| params.rows());
        cx.simulate_input("call(");
        settle(cx, |cx| !rows(cx).is_empty());
        assert_eq!(rows(cx), ["> call([a], b)"]);
        cx.simulate_input("1,");
        settle(cx, |cx| rows(cx) == ["> call(a, [b])"]);
        cx.simulate_keystrokes("escape");
        assert!(rows(cx).is_empty());
        f.put_caret(cx, 5);
        f.run(cx, "code.parameterInfo");
        settle(cx, |cx| !rows(cx).is_empty());
        assert_eq!(rows(cx), ["> call([a], b)"]);
        f.close(cx);
    }

    /// Code › Show Context Actions lists the server's actions, quick fixes first; the selection's
    /// preview resolves a lazy action once; Enter applies the chosen action's edit through the
    /// backend; with no actions the hint says so.
    #[gpui_kit::test]
    async fn show_intentions_lists_the_code_actions_and_applies_the_chosen_one(cx: &mut TestAppContext) {
        let (f, cx) = open(cx, &[("a.mock", "let foo = 1\n")], "a.mock", json!({}));
        let uri = format!("file://{}", f.root.join("a.mock").display());
        let rename = json!({
            "title": "Rename to bar",
            "kind": "quickfix",
            "edit": { "changes": { uri.clone(): [{ "range": range(0, 4, 7), "newText": "bar" }] } },
        });
        let lazy = json!({ "title": "Inline variable", "kind": "refactor.inline", "data": { "id": 7 } });
        let resolved = json!({
            "title": "Inline variable",
            "kind": "refactor.inline",
            "edit": { "changes": { uri: [{ "range": range(0, 0, 11), "newText": "1" }] } },
        });
        f.server.answer(json!({ "textDocument/codeAction": [lazy, rename], "codeAction/resolve": resolved }));
        f.put_caret(cx, 5);
        f.run(cx, "code.showIntentions");
        let intelligence = f.intelligence(cx);
        settle(cx, |cx| intelligence.read_with(cx, |this, _| this.intentions().is_some()));
        let list = intelligence.read_with(cx, |this, _| this.intentions()).expect("the intentions");
        let lines = list.read_with(cx, |list, _| list.lines());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("Rename to bar") && lines[1].contains("Inline variable"), "{lines:?}");
        assert!(
            cx.update(|window, cx| list.focus_handle(cx).contains_focused(window, cx)),
            "the list has the keyboard"
        );
        // The lazy action's preview asks the server to resolve it.
        cx.simulate_keystrokes("down");
        settle(cx, |_| !f.requests("codeAction/resolve").is_empty());
        settle(cx, |cx| list.read_with(cx, |list, _| list.preview_text().is_some()));
        cx.simulate_keystrokes("up enter");
        settle(cx, |cx| f.text(cx) == "let bar = 1\n");
        assert!(intelligence.read_with(cx, |this, _| this.intentions().is_none()));

        // A lazy action chosen at once is resolved by the backend as it is applied.
        f.run(cx, "code.showIntentions");
        settle(cx, |cx| intelligence.read_with(cx, |this, _| this.intentions().is_some()));
        cx.simulate_keystrokes("down enter");
        settle(cx, |cx| f.text(cx) == "1\n");

        f.server.answer(json!({ "textDocument/codeAction": [] }));
        f.run(cx, "code.showIntentions");
        settle(cx, |cx| intelligence.read_with(cx, |this, _| this.hint() == Some("No intentions available")));
        f.close(cx);
    }

    /// Escape closes the lookup before the Find bar, and the bar with the next one; the lookup
    /// closes when the editor loses the keyboard.
    #[gpui_kit::test]
    async fn escape_closes_the_lookup_before_the_find_bar(cx: &mut TestAppContext) {
        let answers = json!({ "textDocument/completion": [item("fooBar", 6)] });
        let (f, cx) = open(cx, &[("a.mock", "")], "a.mock", answers);
        cx.simulate_input("obj.");
        settle(cx, |cx| !f.rows(cx).is_empty());
        // The Find bar takes the keyboard: the lookup closes.
        f.workbench.update_in(cx, |workbench, window, cx| workbench.show_find_bar(false, window, cx));
        cx.run_until_parked();
        assert!(f.rows(cx).is_empty());
        let editor = f.editor(cx).expect("an editor");
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        f.run(cx, "code.completion");
        settle(cx, |cx| !f.rows(cx).is_empty());
        let bar_open =
            |cx: &mut VisualTestContext| f.workbench.read_with(cx, |workbench, _| workbench.find_bar().is_some());
        assert!(bar_open(cx));
        cx.simulate_keystrokes("escape");
        assert!(f.rows(cx).is_empty());
        assert!(bar_open(cx), "the first Escape closed the lookup only");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!bar_open(cx));
        assert_eq!(f.text(cx), "obj.");
        f.close(cx);
    }
}
