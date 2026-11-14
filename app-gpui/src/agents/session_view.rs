//! A task's conversation: the user's messages and the agent's replies (drawn from their Markdown), the
//! author heading over each run of the agent's work, folded runs of reasoning and tool calls that open and
//! close, live tool calls with their output, plans, subagent cards that open the subagent's conversation,
//! permission requests answered from their line, the footer of each message (its time and its actions),
//! a sent message edited in place, the meters, the `AskUserQuestion` card above the composer, and a slot
//! for the composer.
//!
//! Scrolling follows the end while output streams in, until the user scrolls away; Jump to Latest goes
//! back. [`Follow`] decides: the view feeds it what it measured on the last frame and applies what it
//! answers.
//!
//! [`SessionView`] owns no agent process. It reads the [`AgentsStore`]; what needs the backend comes out as
//! [`SessionEvent::Effects`] (answers to permission requests, with tickets whose results come back through
//! [`SessionView::answered`]), what the reply actions do as [`SessionEvent::Message`], and links as
//! [`SessionEvent::Link`].

use crate::agents::cards::{
    Choice, PermissionLine, QuestionForm, Responding, permission_line, question_card, short_title,
};
use crate::agents::composer::{MessageToken, TokenKind, message_tokens};
use crate::agents::follow::{Follow, Geometry, ScrollEffect};
use crate::agents::markdown::{Block, LinkAction, MarkdownCache};
use crate::agents::markdown_view::{MarkdownClick, MarkdownHandler, markdown_element};
use crate::agents::message_editor::{EditAction, EditKey, KeyState, MessageEditor, replaces_later};
use crate::agents::model::{
    find_subagent, is_busy, provider_icon, reply_times, reply_turns, subagent_status_label, subagent_title,
    turn_numbers,
};
use crate::agents::permission::ButtonVariant;
use crate::agents::reply_actions::{COPIED_MS, CopyFeedback, ReplyTarget};
use crate::agents::session::{
    ConversationRow, Fold, Folds, MessageTime, Meter, ProcessSummary, ReplyButton, SessionNav, UserButton,
    author_headers, conversation_rows, message_time, meters, process_summary, reads_conversation, rendered_texts,
    reply_buttons, thought_label, user_buttons, user_files,
};
use crate::agents::store::{AgentEffect, AgentsStore, Ticket};
use crate::platform::i18n::{I18n, Localization};
use crate::platform::input;
use crate::platform::local_time;
use crate::theme;
use crate::ui::text_field::{TextField, TextFieldEvent};
use crate::workbench::notifications_view::local_time_label;
use gpui_kit::{
    AnyElement, AnyView, App, ClipboardItem, Context, Div, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, Font, FontWeight, Hsla, IntoElement, KeyDownEvent, Keystroke, MouseButton, Render, ScrollHandle,
    SharedString, Stateful, StyledText, Subscription, Task, TextRun, Window, div, font, point, prelude::*, px,
};
use ide_agent::{AgentEntry, AgentImage, AgentQuestion, AgentTask, AgentToolStatus};
use ide_core::settings::AgentSettings;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// How a line break of a message being edited is shown, and typed, in its one-line field.
const LINE_BREAK_MARK: &str = "⏎";

fn marked(text: &str) -> String {
    text.replace("\r\n", "\n").replace(['\n', '\r'], LINE_BREAK_MARK)
}

fn unmarked(text: &str) -> String {
    text.replace(LINE_BREAK_MARK, "\n")
}

/// UI text in the active language.
fn tx(cx: &App, text: &str) -> String {
    cx.try_global::<Localization>().map_or_else(|| text.to_owned(), |localization| localization.0.tx(text))
}

fn ui_font(cx: &App) -> Font {
    let family = cx
        .try_global::<theme::ActiveTheme>()
        .and_then(|active| active.theme.font_families("font-ui").into_iter().next())
        .unwrap_or_else(|| ".SystemUIFont".to_owned());
    font(SharedString::from(family))
}

fn mono_font(cx: &App) -> Font {
    let family = cx
        .try_global::<theme::ActiveTheme>()
        .and_then(|active| active.theme.font_families("font-mono").into_iter().next())
        .unwrap_or_else(|| theme::fonts::BUNDLED_MONO_FAMILY.to_owned());
    font(SharedString::from(family))
}

/// What a reply or message action asks the owner to do; the owner runs it through
/// [`crate::agents::reply_actions::ReplyActions`] or the store.
#[derive(Clone, Debug, PartialEq)]
pub enum MessageAction {
    /// Quote Reply: the reply's Markdown goes into the task's composer.
    Quote { task: String, text: String },
    /// Regenerate Response.
    Regenerate(ReplyTarget),
    /// Branch Conversation from Here.
    Branch(ReplyTarget),
    /// Retract Message.
    Retract(ReplyTarget),
    /// Run Again From Here: the turn runs again with the message as sent.
    RunAgain { target: ReplyTarget, text: String, images: Vec<AgentImage> },
    /// Send Again: the message is sent once more, as a new turn.
    SendAgain { task: String, text: String },
    /// Copy to a New Task From Here.
    Fork(ReplyTarget),
    /// The edited message is sent: the conversation runs again from it with this text and these images
    /// ([`AgentsStore::rerun_from`]); the owner reports the end with [`SessionView::edit_sent`].
    EditSend { target: ReplyTarget, text: String, images: Vec<AgentImage> },
}

/// What a conversation asks of its window.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// Carry out these effects; the answers of their calls come back through [`SessionView::answered`].
    Effects(Vec<AgentEffect>),
    Message(MessageAction),
    /// A link of a message was clicked.
    Link(LinkAction),
}

fn reply_label(button: ReplyButton, copied: bool) -> &'static str {
    match button {
        ReplyButton::Copy if copied => "Copied",
        ReplyButton::Copy => "Copy Markdown",
        ReplyButton::QuoteReply => "Quote Reply",
        ReplyButton::Regenerate => "Regenerate Response",
        ReplyButton::Branch => "Branch Conversation from Here",
    }
}

fn reply_slug(button: ReplyButton) -> &'static str {
    match button {
        ReplyButton::Copy => "copy",
        ReplyButton::QuoteReply => "quote",
        ReplyButton::Regenerate => "regenerate",
        ReplyButton::Branch => "branch",
    }
}

fn reply_icon(button: ReplyButton, copied: bool) -> &'static str {
    match button {
        ReplyButton::Copy if copied => "check",
        ReplyButton::Copy => "copy",
        ReplyButton::QuoteReply => "quote",
        ReplyButton::Regenerate => "refresh",
        ReplyButton::Branch => "branch",
    }
}

fn user_label(button: UserButton) -> &'static str {
    match button {
        UserButton::Copy => "Copy",
        UserButton::Edit => "Edit",
        UserButton::Retract => "Retract Message",
        UserButton::RunAgainFromHere => "Run Again From Here",
        UserButton::SendAgain => "Send Again",
        UserButton::Fork => "Copy to a New Task From Here",
    }
}

fn user_slug(button: UserButton) -> &'static str {
    match button {
        UserButton::Copy => "copy",
        UserButton::Edit => "edit",
        UserButton::Retract => "retract",
        UserButton::RunAgainFromHere => "run-again",
        UserButton::SendAgain => "send-again",
        UserButton::Fork => "fork",
    }
}

fn user_icon(button: UserButton) -> &'static str {
    match button {
        UserButton::Copy => "copy",
        UserButton::Edit => "edit",
        UserButton::Retract => "diff-revert",
        UserButton::RunAgainFromHere | UserButton::SendAgain => "refresh",
        UserButton::Fork => "branch",
    }
}

fn time_text(time: Option<&MessageTime>) -> Option<String> {
    let time = time?;
    let clock = local_time_label(time.at_ms);
    Some(match &time.took {
        Some(took) => format!("{clock} · {took}"),
        None => clock,
    })
}

/// `+12` / `−3`, in the colour of token `color`.
fn counted(cx: &App, color: &str, sign: char, n: u32) -> Div {
    div().text_color(theme::color(cx, color)).child(SharedString::from(format!("{sign}{n}")))
}

/// A small caption.
fn small(cx: &App, color: Hsla, text: String) -> Div {
    div().text_color(color).text_size(theme::length(cx, "fs-small")).child(SharedString::from(text))
}

fn tool_icon(status: AgentToolStatus) -> (&'static str, &'static str) {
    match status {
        AgentToolStatus::Pending | AgentToolStatus::Running => ("refresh", "fg-muted"),
        AgentToolStatus::Completed => ("check", "success"),
        AgentToolStatus::Failed => ("close", "danger"),
    }
}

// ----- what the rows show -----

#[derive(Clone, Debug)]
struct UserRow {
    text: String,
    tokens: Vec<MessageToken>,
    files: Vec<String>,
    images: Vec<AgentImage>,
    time: Option<MessageTime>,
    buttons: Vec<UserButton>,
    turn: Option<usize>,
}

#[derive(Clone, Debug)]
struct AgentRow {
    text: String,
    blocks: Arc<Vec<Block>>,
    time: Option<MessageTime>,
    buttons: Vec<ReplyButton>,
    turn: Option<usize>,
    copied: bool,
}

#[derive(Clone, Debug)]
struct ToolRow {
    title: String,
    status: AgentToolStatus,
    /// The output, while the row renders it.
    output: Option<String>,
    has_output: bool,
    open: bool,
    added: u32,
    removed: u32,
}

#[derive(Clone, Debug)]
enum RowKind {
    User(Box<UserRow>),
    Agent(Box<AgentRow>),
    /// A reasoning entry: its heading, and its text while it renders.
    Thought {
        label: String,
        text: Option<String>,
        open: bool,
    },
    Tool(Box<ToolRow>),
    Process {
        summary: ProcessSummary,
        open: bool,
        children: Vec<RowModel>,
    },
    Plan(Vec<(String, String)>),
    Subagent {
        id: String,
        title: String,
        status: String,
        activity: Option<String>,
    },
    Message {
        incoming: bool,
        text: String,
        blocks: Arc<Vec<Block>>,
    },
    Permission {
        id: u32,
        line: Box<PermissionLine>,
    },
    Error(String),
}

#[derive(Clone, Debug)]
struct RowModel {
    /// The index of the row's (first) entry in the conversation.
    index: usize,
    /// The row starts a run of the agent's work and carries its author heading.
    header: bool,
    kind: RowKind,
}

/// What a render draws, built from the store with the view's state.
#[derive(Default)]
struct Snapshot {
    task: Option<AgentTask>,
    provider: String,
    cwd: String,
    rows: Vec<RowModel>,
    meters: Vec<String>,
    /// The conversation's title, then each subagent's on the way to the one shown.
    crumbs: Vec<String>,
    jump: bool,
    /// The number of entries and the size of the last one: a change means the rows changed.
    signature: (usize, usize),
}

/// The part of the view a snapshot is built from.
struct Inputs<'a> {
    nav: &'a SessionNav,
    folds: &'a Folds,
    follow: &'a Follow,
    copy: &'a CopyFeedback,
    finding: bool,
    settings: &'a AgentSettings,
    now: f64,
}

/// What a row is built from.
struct RowContext<'a> {
    task: &'a AgentTask,
    entries: &'a [AgentEntry],
    inputs: &'a Inputs<'a>,
    working: Option<usize>,
    turns: &'a std::collections::BTreeMap<usize, usize>,
    turn_count: usize,
    replies: &'a std::collections::BTreeMap<usize, crate::agents::model::ReplyTurn<'a>>,
    times: &'a std::collections::BTreeMap<usize, crate::agents::model::ReplyTimes>,
    provider: &'a str,
}

fn entry_size(entry: &AgentEntry) -> usize {
    match entry {
        AgentEntry::User { text, .. }
        | AgentEntry::Agent { text }
        | AgentEntry::Thought { text }
        | AgentEntry::Message { text, .. }
        | AgentEntry::Error { message: text } => text.len(),
        AgentEntry::Tool { output, status, .. } => output.as_ref().map_or(0, String::len) + *status as usize,
        AgentEntry::Plan { items } => items.iter().map(|item| item.content.len() + item.status.len()).sum(),
        AgentEntry::Subagent { tools, ended_ms, .. } => *tools as usize + usize::from(ended_ms.is_some()),
        AgentEntry::Permission { chosen, .. } => chosen.as_ref().map_or(0, |c| c.len() + 1),
    }
}

fn build(store: &AgentsStore, inputs: &Inputs, cache: &mut MarkdownCache, i18n: &mut I18n) -> Snapshot {
    let nav = inputs.nav;
    let Some(task) = store.task(nav.task()) else { return Snapshot::default() };
    let entries = store.entries(nav.task(), nav.subagent()).unwrap_or_default();
    let provider = store.provider(&task.provider).map_or_else(|| task.provider.clone(), |p| p.name.clone());
    let start = inputs.follow.start(entries, inputs.finding);
    let rows = conversation_rows(entries, start);
    let headers = author_headers(entries, &rows);
    let working = (is_busy(task.status) && !entries.is_empty()).then(|| entries.len() - 1);
    let turns = turn_numbers(entries);
    let replies = reply_turns(entries);
    let times = reply_times(entries);
    let context = RowContext {
        task,
        entries,
        inputs,
        working,
        turns: &turns,
        turn_count: turns.len(),
        replies: &replies,
        times: &times,
        provider: &provider,
    };
    let mut models = Vec::with_capacity(rows.len());
    for row in &rows {
        models.push(row_model(row, headers.contains(&row.index()), &context, cache, i18n));
    }
    let own = store.entries(nav.task(), None);
    let meters = meters(task, own, store.limits(&task.provider), inputs.settings, inputs.now, i18n)
        .into_iter()
        .map(|meter| match meter {
            Meter::Context(label) => label.short,
            Meter::Limit(label) => label.short,
            Meter::Cost(text) | Meter::WorkTime(text) => text,
        })
        .collect();
    let mut crumbs = vec![task.title.clone()];
    for (depth, id) in nav.path().iter().enumerate() {
        let parent = depth.checked_sub(1).and_then(|d| nav.path().get(d)).map(String::as_str);
        let card = find_subagent(store.entries(nav.task(), parent), id);
        crumbs.push(card.map_or_else(|| id.clone(), |card| subagent_title(card).to_owned()));
    }
    Snapshot {
        task: Some(task.clone()),
        cwd: task.cwd.0.to_string_lossy().into_owned(),
        provider,
        rows: models,
        meters,
        crumbs,
        jump: inputs.follow.shows_jump_to_latest(entries),
        signature: (entries.len(), entries.last().map_or(0, entry_size)),
    }
}

fn row_model(
    row: &ConversationRow,
    header: bool,
    context: &RowContext,
    cache: &mut MarkdownCache,
    i18n: &mut I18n,
) -> RowModel {
    match row {
        ConversationRow::Entry { index } => {
            RowModel { index: *index, header, kind: entry_kind(*index, context, cache, i18n) }
        }
        ConversationRow::Process { index, entries: run } => {
            let (entries, folds, finding) = (context.entries, context.inputs.folds, context.inputs.finding);
            let fold = Fold::Process(*index);
            let summary = process_summary(entries, run, context.working, i18n);
            let mut children = Vec::new();
            if folds.renders(fold, entries, finding) {
                for i in run {
                    children.push(RowModel { index: *i, header: false, kind: entry_kind(*i, context, cache, i18n) });
                }
            }
            let open = folds.is_open(fold, entries);
            RowModel { index: *index, header, kind: RowKind::Process { summary, open, children } }
        }
    }
}

fn entry_kind(index: usize, context: &RowContext, cache: &mut MarkdownCache, i18n: &mut I18n) -> RowKind {
    let RowContext { task, entries, inputs, .. } = *context;
    let Some(entry) = entries.get(index) else { return RowKind::Error(String::new()) };
    match entry {
        AgentEntry::User { text, attachments, images, sent_ms, done_ms, .. } => {
            let turn = context.turns.get(&index).copied();
            RowKind::User(Box::new(UserRow {
                text: text.clone(),
                tokens: message_tokens(text),
                files: user_files(attachments, images.len()).to_vec(),
                images: images.clone(),
                time: message_time(*sent_ms, *done_ms, false),
                buttons: user_buttons(task, text, attachments.len(), turn, context.turn_count),
                turn,
            }))
        }
        AgentEntry::Agent { text } => {
            let reply = context.replies.get(&index);
            let mut buttons = reply_buttons(task, reply, context.turn_count);
            if CopyFeedback::copy(text).is_none() {
                buttons.retain(|b| *b != ReplyButton::Copy);
            }
            let time = context.times.get(&index).and_then(|t| message_time(t.sent_ms, t.done_ms, true));
            RowKind::Agent(Box::new(AgentRow {
                text: text.clone(),
                blocks: cache.blocks(text),
                time,
                buttons,
                turn: reply.map(|r| r.turn),
                copied: inputs.copy.shows_copied(text, inputs.now),
            }))
        }
        AgentEntry::Thought { text } => {
            let fold = Fold::Thought(index);
            RowKind::Thought {
                label: thought_label(context.working == Some(index), i18n),
                text: inputs.folds.renders(fold, entries, inputs.finding).then(|| text.clone()),
                open: inputs.folds.is_open(fold, entries),
            }
        }
        AgentEntry::Tool { title, kind, status, output, diffs, .. } => {
            let fold = Fold::Tool(index);
            let title = if title.is_empty() { kind } else { title };
            RowKind::Tool(Box::new(ToolRow {
                title: short_title(task, title),
                status: *status,
                output: output.clone().filter(|_| inputs.folds.renders(fold, entries, inputs.finding)),
                has_output: output.is_some(),
                open: inputs.folds.is_open(fold, entries),
                added: diffs.iter().map(|d| d.added).sum(),
                removed: diffs.iter().map(|d| d.removed).sum(),
            }))
        }
        AgentEntry::Plan { items } => {
            RowKind::Plan(items.iter().map(|item| (item.content.clone(), item.status.clone())).collect())
        }
        AgentEntry::Subagent { id, status, activity, .. } => RowKind::Subagent {
            id: id.clone(),
            title: subagent_title(entry).to_owned(),
            status: i18n.tx(subagent_status_label(*status)),
            activity: activity.clone(),
        },
        AgentEntry::Message { incoming, text } => {
            RowKind::Message { incoming: *incoming, text: text.clone(), blocks: cache.blocks(text) }
        }
        AgentEntry::Permission { id, .. } => match permission_line(entry, task, context.provider, i18n) {
            Some(line) => RowKind::Permission { id: *id, line: Box::new(line) },
            None => RowKind::Error(String::new()),
        },
        AgentEntry::Error { message } => RowKind::Error(message.clone()),
    }
}

/// What a row says, as [`SessionView::lines`] lists it.
fn row_lines(row: &RowModel, indent: &str, provider: &str, out: &mut Vec<String>) {
    if row.header {
        out.push(format!("{indent}author: {provider}"));
    }
    match &row.kind {
        RowKind::User(user) => {
            let mut line = format!("{indent}you: {}", user.text);
            if !user.files.is_empty() {
                line.push_str(&format!(" [files: {}]", user.files.join(", ")));
            }
            if !user.images.is_empty() {
                line.push_str(&format!(" [images: {}]", user.images.len()));
            }
            if let Some(took) = user.time.as_ref().and_then(|t| t.took.as_deref()) {
                line.push_str(&format!(" | {took}"));
            }
            out.push(line);
        }
        RowKind::Agent(agent) => {
            let took = agent.time.as_ref().and_then(|t| t.took.as_deref()).map(|t| format!(" | {t}"));
            out.push(format!("{indent}agent: {}{}", agent.text, took.unwrap_or_default()));
        }
        RowKind::Thought { label, text, .. } => {
            let text = text.as_deref().map(|t| format!(" | {t}")).unwrap_or_default();
            out.push(format!("{indent}thought: {label}{text}"));
        }
        RowKind::Tool(tool) => {
            let output = tool.output.as_deref().map(|o| format!(" | {o}")).unwrap_or_default();
            out.push(format!("{indent}tool: {}{output}", tool.title));
        }
        RowKind::Process { summary, open, children } => {
            out.push(format!("{indent}fold: {}{}", summary.label, if *open { " (open)" } else { "" }));
            let inner = format!("{indent}  ");
            for child in children {
                row_lines(child, &inner, provider, out);
            }
        }
        RowKind::Plan(items) => {
            let items: Vec<String> = items.iter().map(|(content, status)| format!("{content} ({status})")).collect();
            out.push(format!("{indent}plan: {}", items.join(", ")));
        }
        RowKind::Subagent { title, status, .. } => out.push(format!("{indent}subagent: {title} ({status})")),
        RowKind::Message { incoming, text, .. } => {
            out.push(format!("{indent}{}: {text}", if *incoming { "task" } else { "report" }));
        }
        RowKind::Permission { line, .. } => match &line.record {
            Some(record) => out.push(format!("{indent}permission: {}: {}", record.label, record.subject)),
            None => {
                let mut text = format!("{indent}permission: {}", line.title);
                if let Some(command) = &line.command {
                    text.push_str(&format!(" | {command}"));
                }
                for answer in &line.answers {
                    text.push_str(&format!(" [{}]", answer.label));
                }
                out.push(text);
            }
        },
        RowKind::Error(message) => out.push(format!("{indent}error: {message}")),
    }
}

fn find_row(rows: &[RowModel], index: usize) -> Option<&RowModel> {
    rows.iter().find_map(|row| {
        if row.index == index && !matches!(row.kind, RowKind::Process { .. }) {
            return Some(row);
        }
        match &row.kind {
            RowKind::Process { children, .. } => find_row(children, index),
            _ => None,
        }
    })
}

/// The `AskUserQuestion` card: its form, the typed-answer field of each question, and where its keys go.
struct QuestionState {
    form: QuestionForm,
    fields: Vec<Entity<TextField>>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

/// A sent message being edited in place.
struct EditState {
    index: usize,
    turn: usize,
    editor: MessageEditor,
    field: Entity<TextField>,
    claim: u64,
    _subscription: Subscription,
}

/// A scroll offset the view set itself: the next frame's offset there is not the user's.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expected {
    Nothing,
    Top(f64),
    End,
}

pub struct SessionView {
    store: Entity<AgentsStore>,
    nav: SessionNav,
    docked: bool,
    shown: bool,
    follow: Follow,
    folds: Folds,
    finding: bool,
    cache: MarkdownCache,
    copy: CopyFeedback,
    responding: Responding,
    question: Option<QuestionState>,
    editing: Option<EditState>,
    settings: AgentSettings,
    composer: Option<AnyView>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    clock: fn() -> f64,
    handler: MarkdownHandler,
    /// The entry index of each row painted last, in order: the scroll container's children.
    painted: Vec<usize>,
    /// What the last render drew ([`Snapshot::signature`]).
    drawn: Option<(usize, usize)>,
    /// The offset seen on the last frame.
    seen_top: Option<f64>,
    expected: Expected,
    /// The row that stays on screen while earlier rows are prepended above it.
    anchor_row: Option<usize>,
    /// The follow timer and the end of the Copied feedback, kept while they wait.
    _wake: Option<Task<()>>,
    _copied_timer: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SessionEvent> for SessionView {}

impl SessionView {
    /// The conversation of `task`, in an editor tab or docked in the tool window (`docked`). The owner
    /// subscribes, then calls [`Self::load`].
    pub fn new(store: Entity<AgentsStore>, task: &str, docked: bool, cx: &mut Context<Self>) -> Self {
        let nav = SessionNav::new(task);
        let follow = Self::follow_for(&store, &nav, cx);
        let weak = cx.weak_entity();
        let handler: MarkdownHandler = Rc::new(move |click: &MarkdownClick, _: &mut Window, cx: &mut App| {
            let click = click.clone();
            let _ = weak.update(cx, |view: &mut Self, cx| view.markdown_clicked(click, cx));
        });
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        cx.on_release(|view: &mut Self, cx| view.closed(cx)).detach();
        Self {
            store,
            nav,
            docked,
            shown: false,
            follow,
            folds: Folds::default(),
            finding: false,
            cache: MarkdownCache::default(),
            copy: CopyFeedback::default(),
            responding: Responding::default(),
            question: None,
            editing: None,
            settings: AgentSettings::default(),
            composer: None,
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            clock: local_time::now_ms,
            handler,
            painted: Vec::new(),
            drawn: None,
            seen_top: None,
            expected: Expected::Nothing,
            anchor_row: None,
            _wake: None,
            _copied_timer: None,
            _subscriptions: subscriptions,
        }
    }

    /// The clock follow timers and the meters run on.
    pub fn with_clock(mut self, clock: fn() -> f64) -> Self {
        self.clock = clock;
        self
    }

    fn follow_for(store: &Entity<AgentsStore>, nav: &SessionNav, cx: &App) -> Follow {
        let store = store.read(cx);
        let len = store.entries(nav.task(), nav.subagent()).map_or(0, <[AgentEntry]>::len);
        Follow::open(store.scroll_position(&nav.entries_key()), len)
    }

    /// Reads the conversation shown, unless the store has it.
    pub fn load(&mut self, cx: &mut Context<Self>) {
        let effect = self.store.read(cx).open_conversation(self.nav.task(), self.nav.subagent());
        if let Some(effect) = effect {
            cx.emit(SessionEvent::Effects(vec![effect]));
        }
    }

    /// The task the view shows.
    pub fn task(&self) -> &str {
        self.nav.task()
    }

    /// The docked task changed: another task opens at its own conversation.
    pub fn show_task(&mut self, task: &str, cx: &mut Context<Self>) {
        if self.nav.task() == task && self.nav.path().is_empty() {
            return;
        }
        let was_shown = self.shown;
        self.set_shown(false, cx);
        self.leave_conversation(cx);
        self.nav.show_task(task);
        self.nav.navigate(0);
        self.enter_conversation(cx);
        self.set_shown(was_shown, cx);
        self.load(cx);
    }

    /// Whether the conversation is on screen (its tab is the active one, or the tool window shows it):
    /// a shown conversation counts as read.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        let (task, docked) = (self.nav.task().to_owned(), self.docked);
        self.store.update(cx, |store, cx| {
            if reads_conversation(shown, docked, false) {
                store.show_session(&task);
            } else {
                store.hide_session(&task);
            }
            cx.notify();
        });
    }

    /// Settings › Tools › Agents › Chat: which meters show.
    pub fn set_agent_settings(&mut self, settings: AgentSettings, cx: &mut Context<Self>) {
        self.settings = settings;
        cx.notify();
    }

    /// The composer drawn under the conversation, while it takes messages.
    pub fn set_composer(&mut self, composer: Option<AnyView>, cx: &mut Context<Self>) {
        self.composer = composer;
        cx.notify();
    }

    /// Find in Conversation is open: every fold renders its content (without opening), so its text can be
    /// found, and the whole conversation is painted.
    pub fn set_finding(&mut self, finding: bool, cx: &mut Context<Self>) {
        self.finding = finding;
        cx.notify();
    }

    /// The text the rows render, in order, as Find in Conversation searches it.
    pub fn rendered_texts(&self, cx: &App) -> Vec<String> {
        let store = self.store.read(cx);
        let Some(task) = store.task(self.nav.task()) else { return Vec::new() };
        let entries = store.entries(self.nav.task(), self.nav.subagent()).unwrap_or_default();
        let rows = conversation_rows(entries, self.follow.start(entries, self.finding));
        let working = (is_busy(task.status) && !entries.is_empty()).then(|| entries.len() - 1);
        let english = I18n::default();
        let i18n = cx.try_global::<Localization>().map_or(&english, |localization| &localization.0);
        rendered_texts(entries, &rows, &self.folds, self.finding, working, i18n)
    }

    /// The answer of a call this view asked for (a permission answer, the question card's answer); one for
    /// another request is dropped.
    pub fn answered(&mut self, ticket: Ticket, result: Result<(), String>, cx: &mut Context<Self>) {
        self.responding.answered(ticket, result.clone());
        if let Some(question) = &mut self.question {
            question.form.answered(ticket, result);
        }
        cx.notify();
    }

    /// The edited message was sent ([`MessageAction::EditSend`]): on success the editor closes; a failure
    /// keeps the edit with its message beside it.
    pub fn edit_sent(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        let ok = result.is_ok();
        if let Some(edit) = &mut self.editing {
            edit.editor.sent(result);
        }
        if ok {
            self.close_editor(cx);
        }
        cx.notify();
    }

    // ----- accessors -----

    fn snapshot(&mut self, cx: &mut Context<Self>) -> Snapshot {
        let inputs = Inputs {
            nav: &self.nav,
            folds: &self.folds,
            follow: &self.follow,
            copy: &self.copy,
            finding: self.finding,
            settings: &self.settings,
            now: (self.clock)(),
        };
        let cache = &mut self.cache;
        self.store.update(cx, |store, cx| build(store, &inputs, cache, &mut cx.default_global::<Localization>().0))
    }

    /// What the conversation shows, a line per row (the rows of an open fold indented under it):
    /// `author: <agent>` over a run of its work, `you: <text>` (with ` [files: …]`, ` [images: n]` and
    /// ` | <took>`), `agent: <markdown> | <took>`, `fold: Reasoning · 2 tool calls (open)`,
    /// `thought: Reasoning | <text>`, `tool: <title> | <output>`, `plan: <item> (<status>), …`,
    /// `subagent: <title> (<status>)`, `task: …` / `report: …`, `permission: <title> | <command> [<answer>]…`
    /// (`permission: <record>: <subject>` once answered) and `error: <message>`.
    pub fn lines(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        let snapshot = self.snapshot(cx);
        let mut out = Vec::new();
        for row in &snapshot.rows {
            row_lines(row, "", &snapshot.provider, &mut out);
        }
        out
    }

    /// The labels of the footer actions of the message at entry `index`.
    pub fn buttons(&mut self, index: usize, cx: &mut Context<Self>) -> Vec<String> {
        let snapshot = self.snapshot(cx);
        match find_row(&snapshot.rows, index).map(|row| &row.kind) {
            Some(RowKind::Agent(agent)) => {
                agent.buttons.iter().map(|b| tx(cx, reply_label(*b, agent.copied))).collect()
            }
            Some(RowKind::User(user)) => user.buttons.iter().map(|b| tx(cx, user_label(*b))).collect(),
            _ => Vec::new(),
        }
    }

    /// The meters of the header, as shown: `12%`, `5h 19%`, `$0.42`, `3m`.
    pub fn meters(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        self.snapshot(cx).meters
    }

    /// The question card as it reads: per question `<header>: <question>`, then each choice as
    /// `(x) <label>` or `( ) <label>` (with ` [Recommended]`), then `preview: <caption>` when it has one.
    pub fn card(&self, cx: &App) -> Option<Vec<String>> {
        let question = self.question.as_ref()?;
        let english = I18n::default();
        let i18n = cx.try_global::<Localization>().map_or(&english, |localization| &localization.0);
        let mut out = Vec::new();
        for (position, q) in question.form.questions().iter().enumerate() {
            out.push(format!("{}: {}", q.header, q.question));
            for choice in question.form.choices(position, i18n) {
                let mark = if choice.checked { "(x)" } else { "( )" };
                let recommended = if choice.recommended { " [Recommended]" } else { "" };
                out.push(format!("{mark} {}{recommended}", choice.label));
            }
            if let Some(preview) = question.form.preview(position, i18n) {
                out.push(format!("preview: {}", preview.caption));
            }
        }
        Some(out)
    }

    /// Whether the question card's Submit is enabled.
    pub fn can_submit(&self) -> bool {
        self.question.as_ref().is_some_and(|q| q.form.can_submit())
    }

    pub fn shows_jump_to_latest(&self, cx: &App) -> bool {
        let entries = self.store.read(cx).entries(self.nav.task(), self.nav.subagent()).unwrap_or_default();
        self.follow.shows_jump_to_latest(entries)
    }

    /// Whether the conversation follows new output.
    pub fn follows(&self) -> bool {
        self.follow.position().stick
    }

    /// The entry index of the first row painted.
    pub fn first_painted(&self) -> Option<usize> {
        self.painted.first().copied()
    }

    /// The entry index of the message being edited.
    pub fn editing(&self) -> Option<usize> {
        self.editing.as_ref().map(|edit| edit.index)
    }

    /// The text of the message being edited, its line breaks as typed.
    pub fn edit_text(&self) -> Option<&str> {
        self.editing.as_ref().map(|edit| edit.editor.text())
    }

    /// Whether the edited message's Send is enabled.
    pub fn can_send_edit(&self) -> bool {
        self.editing.as_ref().is_some_and(|edit| edit.editor.can_send())
    }

    /// The subagents leading to the conversation shown.
    pub fn path(&self) -> &[String] {
        self.nav.path()
    }

    // ----- conversations -----

    fn leave_conversation(&mut self, cx: &mut Context<Self>) {
        let top = f64::from(-f32::from(self.scroll.offset().y));
        if let Some(position) = self.follow.close(top) {
            let key = self.nav.entries_key();
            self.store.update(cx, |store, _| store.set_scroll_position(&key, position));
        }
        self.close_editor(cx);
    }

    fn enter_conversation(&mut self, cx: &mut Context<Self>) {
        self.follow = Self::follow_for(&self.store, &self.nav, cx);
        self.folds = Folds::default();
        self.painted.clear();
        (self.drawn, self.seen_top, self.expected, self.anchor_row) = (None, None, Expected::Nothing, None);
        self._wake = None;
        self.scroll.set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    fn open_subagent(&mut self, id: &str, cx: &mut Context<Self>) {
        self.leave_conversation(cx);
        let effect = self.nav.open_subagent(id, self.store.read(cx));
        self.enter_conversation(cx);
        if let Some(effect) = effect {
            cx.emit(SessionEvent::Effects(vec![effect]));
        }
    }

    /// A breadcrumb: back to the conversation `depth` subagents deep (0 is the task's own).
    pub fn navigate(&mut self, depth: usize, cx: &mut Context<Self>) {
        if depth >= self.nav.path().len() {
            return;
        }
        self.leave_conversation(cx);
        self.nav.navigate(depth);
        self.enter_conversation(cx);
    }

    fn closed(&mut self, cx: &mut App) {
        let top = f64::from(-f32::from(self.scroll.offset().y));
        if let Some(position) = self.follow.close(top) {
            let key = self.nav.entries_key();
            self.store.update(cx, |store, _| store.set_scroll_position(&key, position));
        }
        if let Some(edit) = self.editing.take() {
            input::release_keys(edit.claim, cx);
        }
        if self.shown {
            let task = self.nav.task().to_owned();
            self.store.update(cx, |store, cx| {
                store.hide_session(&task);
                cx.notify();
            });
        }
    }

    // ----- scrolling -----

    fn geometry(&self) -> Option<Geometry> {
        let bounds = self.scroll.bounds();
        let viewport = f64::from(f32::from(bounds.size.height));
        if viewport <= 0.0 {
            return None;
        }
        let top = f64::from(-f32::from(self.scroll.offset().y));
        let height = f64::from(f32::from(self.scroll.max_offset().y)).max(0.0) + viewport;
        Some(Geometry { top, height, viewport })
    }

    /// The top of the painted row of entry `index`, from the top of the content.
    fn row_top(&self, index: Option<usize>) -> Option<f64> {
        let position = match index {
            Some(index) => self.painted.iter().position(|i| *i == index)?,
            None => 0,
        };
        let row = self.scroll.bounds_for_item(position)?;
        Some(f64::from(f32::from(row.top() - self.scroll.bounds().top())))
    }

    /// Feeds [`Follow`] what the last frame showed and applies its answer; called at the start of every
    /// render with the rows about to be drawn.
    fn sync_scroll(&mut self, signature: (usize, usize), cx: &mut Context<Self>) {
        let now = (self.clock)();
        let mut effects = Vec::new();
        if let Some(g) = self.geometry() {
            let moved = self.seen_top.is_some_and(|seen| (seen - g.top).abs() > 0.5);
            let ours = match self.expected {
                Expected::Nothing => false,
                Expected::Top(top) => (top.clamp(0.0, g.height - g.viewport) - g.top).abs() <= 1.0,
                Expected::End => g.height - g.top - g.viewport <= 1.0,
            };
            self.seen_top = Some(g.top);
            let store = self.store.read(cx);
            let entries = store.entries(self.nav.task(), self.nav.subagent()).unwrap_or_default();
            if moved {
                if !ours {
                    effects.extend(self.follow.scrolled(g, now));
                }
                self.expected = Expected::Nothing;
            }
            // The layout of the rows the last frame drew: only once they stopped changing.
            if self.drawn == Some(signature) {
                let anchor = self.anchor_row.take();
                let first = self.row_top(anchor);
                effects.extend(self.follow.laid_out(g, entries, self.finding, true, first));
            }
        }
        if self.drawn != Some(signature) {
            self.drawn = Some(signature);
            if self.follow.position().stick {
                self.scroll.scroll_to_bottom();
                self.expected = Expected::End;
            }
            self.next_frame(cx);
        }
        self.apply(effects, cx);
    }

    fn next_frame(&self, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = weak.update(cx, |_, cx| cx.notify());
        });
    }

    /// Applies what [`Follow`] answered. An offset takes effect when the rows are next laid out, in this
    /// frame when called from `render`; a caller outside it renders again itself.
    fn apply(&mut self, effects: Vec<ScrollEffect>, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                ScrollEffect::SetTop(top) | ScrollEffect::SmoothTo(top) => {
                    let at_end = self.geometry().is_none_or(|g| top >= g.height - g.viewport - 0.5);
                    if at_end {
                        self.scroll.scroll_to_bottom();
                        self.expected = Expected::End;
                    } else {
                        self.scroll.set_offset(point(px(0.0), px(-(top as f32))));
                        self.expected = Expected::Top(top);
                    }
                }
                ScrollEffect::Save(position) => {
                    let key = self.nav.entries_key();
                    self.store.update(cx, |store, _| store.set_scroll_position(&key, position));
                }
                ScrollEffect::WakeAt(at) => {
                    let delay = Duration::from_millis((at - (self.clock)()).max(0.0) as u64);
                    self._wake = Some(cx.spawn(async move |view, cx| {
                        cx.background_executor().timer(delay).await;
                        let _ = view.update(cx, |view: &mut SessionView, cx| view.due(cx));
                    }));
                }
                ScrollEffect::Rerender => self.next_frame(cx),
            }
        }
    }

    fn due(&mut self, cx: &mut Context<Self>) {
        let now = (self.clock)();
        let first = self.row_top(None);
        let store = self.store.read(cx);
        let entries = store.entries(self.nav.task(), self.nav.subagent()).unwrap_or_default();
        let effect = self.follow.due(now, entries, self.finding, first);
        if effect.is_some() {
            self.anchor_row = self.painted.first().copied();
        }
        self.apply(effect.into_iter().collect(), cx);
        cx.notify();
    }

    /// Jump to Latest: back to the end, following from there.
    pub fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.geometry() else { return };
        let effects = self.follow.jump_to_latest(g);
        self.apply(effects, cx);
        cx.notify();
    }

    fn pointer_up(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.geometry() else { return };
        let effect = self.follow.pointer_up(g, (self.clock)());
        self.apply(effect.into_iter().collect(), cx);
        cx.notify();
    }

    // ----- what clicks do -----

    fn markdown_clicked(&mut self, click: MarkdownClick, cx: &mut Context<Self>) {
        match click {
            MarkdownClick::Link(action) => cx.emit(SessionEvent::Link(action)),
            MarkdownClick::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
        }
    }

    fn toggle_fold(&mut self, fold: Fold, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let entries = store.entries(self.nav.task(), self.nav.subagent()).unwrap_or_default();
        let open = !self.folds.is_open(fold, entries);
        self.folds.toggled(fold, open);
        cx.notify();
    }

    fn target(&self, turn: Option<usize>) -> Option<ReplyTarget> {
        Some(ReplyTarget { task: self.nav.task().to_owned(), turn: turn? })
    }

    fn reply_clicked(&mut self, button: ReplyButton, text: String, turn: Option<usize>, cx: &mut Context<Self>) {
        let action = match button {
            ReplyButton::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                let now = (self.clock)();
                let effect = self.copy.copied(&text, Ok(()), now, &cx.default_global::<Localization>().0);
                if let Some(effect) = effect {
                    cx.emit(SessionEvent::Effects(vec![effect]));
                }
                self._copied_timer = Some(cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(Duration::from_millis(COPIED_MS as u64)).await;
                    let _ = view.update(cx, |_: &mut SessionView, cx| cx.notify());
                }));
                None
            }
            ReplyButton::QuoteReply => Some(MessageAction::Quote { task: self.nav.task().to_owned(), text }),
            ReplyButton::Regenerate => self.target(turn).map(MessageAction::Regenerate),
            ReplyButton::Branch => self.target(turn).map(MessageAction::Branch),
        };
        if let Some(action) = action {
            cx.emit(SessionEvent::Message(action));
        }
        cx.notify();
    }

    fn user_clicked(
        &mut self,
        index: usize,
        button: UserButton,
        user: &UserRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let action = match button {
            UserButton::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(user.text.clone()));
                None
            }
            UserButton::Edit => {
                if let Some(turn) = user.turn {
                    self.start_edit(index, turn, &user.text, &user.images, window, cx);
                }
                None
            }
            UserButton::Retract => self.target(user.turn).map(MessageAction::Retract),
            UserButton::RunAgainFromHere => self.target(user.turn).map(|target| MessageAction::RunAgain {
                target,
                text: user.text.clone(),
                images: user.images.clone(),
            }),
            UserButton::SendAgain => {
                Some(MessageAction::SendAgain { task: self.nav.task().to_owned(), text: user.text.clone() })
            }
            UserButton::Fork => self.target(user.turn).map(MessageAction::Fork),
        };
        if let Some(action) = action {
            cx.emit(SessionEvent::Message(action));
        }
        cx.notify();
    }

    fn permission_clicked(&mut self, id: u32, option: Option<String>, cx: &mut Context<Self>) {
        let task = self.nav.task().to_owned();
        let responding = &mut self.responding;
        let effect = self.store.update(cx, |store, _| responding.answer(store, &task, id, option.as_deref()));
        if let Some(effect) = effect {
            cx.emit(SessionEvent::Effects(vec![effect]));
        }
        cx.notify();
    }

    // ----- editing a sent message -----

    fn start_edit(
        &mut self,
        index: usize,
        turn: usize,
        text: &str,
        images: &[AgentImage],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_editor(cx);
        let shown = marked(text);
        let field = cx.new(|cx| {
            let mut field = TextField::new(cx);
            field.set_text(&shown, cx);
            field
        });
        let subscription = cx.subscribe(&field, |view, field, event: &TextFieldEvent, cx| {
            if *event == TextFieldEvent::Changed {
                let text = unmarked(field.read(cx).text());
                if let Some(edit) = &mut view.editing {
                    edit.editor.set_text(&text);
                }
                cx.notify();
            }
        });
        let weak = cx.weak_entity();
        let claim = input::claim_keys(
            field.read(cx).focus_handle(cx),
            move |keystroke, window, cx| {
                weak.update(cx, |view, cx| view.edit_key(keystroke, window, cx)).unwrap_or(false)
            },
            cx,
        );
        let handle = field.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.editing = Some(EditState {
            index,
            turn,
            editor: MessageEditor::new(text, images),
            field,
            claim,
            _subscription: subscription,
        });
    }

    /// The keys of the edited message's field, ahead of the field: Enter and ⇧Enter type a line break,
    /// ⌘/Ctrl+Enter sends, Escape cancels.
    fn edit_key(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(edit) = &mut self.editing else { return false };
        let key = match keystroke.key.as_str() {
            "enter" => EditKey::Enter,
            "escape" => EditKey::Escape,
            _ => return false,
        };
        let modifiers = keystroke.modifiers;
        let state =
            KeyState { shift: modifiers.shift, command: modifiers.platform || modifiers.control, composing: false };
        let action = edit.editor.key(key, state);
        let (sending, field) = (edit.editor.sending(), edit.field.clone());
        match action {
            Some(EditAction::Send(text, images)) => self.emit_edit(text, images, cx),
            Some(EditAction::Cancel) => self.close_editor(cx),
            // Enter and ⇧Enter type a line break, shown as a mark in the one-line field.
            None if key == EditKey::Enter && !state.command && !sending => {
                field.update(cx, |field, cx| field.replace_text_in_range(None, LINE_BREAK_MARK, window, cx));
            }
            None => {}
        }
        cx.notify();
        true
    }

    fn send_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.editing else { return };
        if let Some(EditAction::Send(text, images)) = edit.editor.send() {
            self.emit_edit(text, images, cx);
        }
        cx.notify();
    }

    fn emit_edit(&self, text: String, images: Vec<AgentImage>, cx: &mut Context<Self>) {
        let Some(edit) = &self.editing else { return };
        let target = ReplyTarget { task: self.nav.task().to_owned(), turn: edit.turn };
        cx.emit(SessionEvent::Message(MessageAction::EditSend { target, text, images }));
    }

    fn remove_edit_image(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(edit) = &mut self.editing {
            edit.editor.remove_image(index);
        }
        cx.notify();
    }

    /// Leaves the editor; the message shows as it was sent.
    pub fn close_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(edit) = self.editing.take() {
            input::release_keys(edit.claim, cx);
            cx.notify();
        }
    }

    // ----- the question card -----

    /// Keeps the card in line with the open `AskUserQuestion` request of the task's own conversation; a new
    /// request gets a new form.
    fn sync_question(&mut self, cx: &mut Context<Self>) {
        let card: Option<(u32, Vec<AgentQuestion>)> = if self.nav.shows_composer() {
            let store = self.store.read(cx);
            store.entries(self.nav.task(), None).and_then(question_card).map(|(id, _, qs)| (id, qs.to_vec()))
        } else {
            None
        };
        let Some((id, questions)) = card else {
            self.question = None;
            return;
        };
        if self.question.as_ref().is_some_and(|q| q.form.permission() == id) {
            return;
        }
        let placeholder = tx(cx, "Other");
        let mut fields = Vec::with_capacity(questions.len());
        let mut subscriptions = Vec::with_capacity(questions.len());
        for position in 0..questions.len() {
            let placeholder = placeholder.clone();
            let field = cx.new(|cx| TextField::new(cx).with_placeholder(placeholder));
            subscriptions.push(cx.subscribe(&field, move |view, field, event: &TextFieldEvent, cx| {
                match event {
                    TextFieldEvent::Changed => {
                        let text = field.read(cx).text().to_owned();
                        if let Some(question) = &mut view.question {
                            question.form.type_other(position, &text);
                        }
                    }
                    TextFieldEvent::Cancelled => view.question_escape(Some(position), cx),
                    TextFieldEvent::Submitted => view.question_submit(cx),
                }
                cx.notify();
            }));
            fields.push(field);
        }
        let task = self.nav.task().to_owned();
        self.question = Some(QuestionState {
            form: QuestionForm::new(&task, id, &questions),
            fields,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        });
    }

    fn choice_clicked(
        &mut self,
        position: usize,
        choice: Choice,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(question) = &mut self.question else { return };
        if question.form.busy() {
            return;
        }
        question.form.focus(position);
        match &choice {
            Choice::Option(label) => {
                question.form.choose(position, label, !checked);
                window.focus(&question.focus, cx);
            }
            Choice::Other => {
                question.form.choose_other(position, !checked);
                if let Some(field) = question.fields.get(position).filter(|_| !checked) {
                    let handle = field.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                }
            }
        }
        cx.notify();
    }

    fn question_submit(&mut self, cx: &mut Context<Self>) {
        let Some(question) = &mut self.question else { return };
        let form = &mut question.form;
        let effect = self.store.update(cx, |store, _| form.submit(store));
        if let Some(effect) = effect {
            cx.emit(SessionEvent::Effects(vec![effect]));
        }
        cx.notify();
    }

    fn question_skip(&mut self, cx: &mut Context<Self>) {
        let Some(question) = &mut self.question else { return };
        let form = &mut question.form;
        let effect = self.store.update(cx, |store, _| form.skip(store));
        if let Some(effect) = effect {
            cx.emit(SessionEvent::Effects(vec![effect]));
        }
        cx.notify();
    }

    /// Escape in the card: clears a typed answer that has text, else skips the request.
    fn question_escape(&mut self, in_other: Option<usize>, cx: &mut Context<Self>) {
        let Some(question) = &mut self.question else { return };
        let form = &mut question.form;
        let effect = self.store.update(cx, |store, _| form.escape(in_other, store));
        if let Some(position) = in_other
            && question.form.other(position) == Some("")
            && let Some(field) = question.fields.get(position)
        {
            field.update(cx, |field, cx| field.set_text("", cx));
        }
        if let Some(effect) = effect {
            cx.emit(SessionEvent::Effects(vec![effect]));
        }
        cx.notify();
    }

    /// Number keys and Escape in the card, outside a typed answer (whose field takes its own keys).
    fn question_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let in_field = self
            .question
            .as_ref()
            .is_some_and(|q| q.fields.iter().any(|field| field.focus_handle(cx).is_focused(window)));
        if in_field {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            self.question_escape(None, cx);
        } else if let Ok(number) = key.parse::<usize>() {
            let Some(question) = &mut self.question else { return };
            if question.form.busy() {
                return;
            }
            question.form.press_number(number);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    // ----- drawing -----

    fn button(
        &self,
        selector: String,
        icon: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let color = theme::color(cx, if enabled { "fg-muted" } else { "fg-faint" });
        let id = selector.clone();
        div()
            .id(SharedString::from(id))
            .debug_selector(move || selector)
            .size(px(22.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::length(cx, "radius-small"))
            .child(theme::icon(cx, icon, 14.0, color))
            .when(enabled, |el| {
                el.hover(|el| el.bg(theme::color(cx, "bg-hover")))
                    .on_click(cx.listener(move |view, _, window, cx| on_click(view, window, cx)))
            })
    }

    fn text_button(
        &self,
        selector: String,
        label: String,
        variant: ButtonVariant,
        enabled: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let id = selector.clone();
        let (bg, fg) = match variant {
            ButtonVariant::Primary => (Some("accent"), "fg-on-accent"),
            ButtonVariant::Secondary => (Some("bg-button"), "fg"),
            ButtonVariant::Ghost => (None, "fg-muted"),
        };
        div()
            .id(SharedString::from(id))
            .debug_selector(move || selector)
            .h(theme::length(cx, "h-control-small"))
            .px(px(8.0))
            .flex()
            .flex_none()
            .items_center()
            .rounded(theme::length(cx, "radius-control"))
            .when_some(bg, |el, bg| el.bg(theme::color(cx, bg)))
            .when(variant == ButtonVariant::Secondary, |el| {
                el.border_1().border_color(theme::color(cx, "border-button"))
            })
            .text_color(theme::color(cx, fg))
            .when(!enabled, |el| el.opacity(0.5))
            .child(SharedString::from(label))
            .when(enabled, |el| el.on_click(cx.listener(move |view, _, window, cx| on_click(view, window, cx))))
    }

    fn author(&self, provider: &str, icon: &str, cx: &App) -> Div {
        let muted = theme::color(cx, "fg-muted");
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_color(muted)
            .text_size(theme::length(cx, "fs-small"))
            .child(theme::icon(cx, icon, 14.0, muted))
            .child(SharedString::from(provider.to_owned()))
    }

    /// A message's footer: its time, then its actions.
    fn footer(&self, time: Option<&MessageTime>, index: usize, buttons: Vec<Stateful<Div>>, cx: &App) -> Div {
        let selector = format!("session-time-{index}");
        let time = time_text(time).map(|text| div().debug_selector(move || selector).child(SharedString::from(text)));
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(theme::length(cx, "fs-small"))
            .text_color(theme::color(cx, "fg-faint"))
            .children(time)
            .child(div().flex().gap(px(2.0)).children(buttons))
    }

    fn user_text(&self, user: &UserRow, cx: &App) -> StyledText {
        let base = ui_font(cx);
        let mut text = String::new();
        let mut runs = Vec::with_capacity(user.tokens.len());
        for token in &user.tokens {
            if token.value.is_empty() {
                continue;
            }
            text.push_str(&token.value);
            let (color, background) = match token.kind {
                TokenKind::Text => (theme::color(cx, "fg"), None),
                TokenKind::File => (theme::color(cx, "link"), Some(theme::color(cx, "bg-hover"))),
                TokenKind::Command => (theme::color(cx, "accent"), Some(theme::color(cx, "bg-hover"))),
            };
            runs.push(TextRun {
                len: token.value.len(),
                font: base.clone(),
                color,
                background_color: background,
                underline: None,
                strikethrough: None,
            });
        }
        if text.is_empty() {
            text.push(' ');
            runs.push(TextRun {
                len: 1,
                font: base,
                color: theme::color(cx, "fg"),
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
        StyledText::new(text).with_runs(runs)
    }

    fn user_element(&self, index: usize, user: &UserRow, cx: &mut Context<Self>) -> Div {
        if self.editing.as_ref().is_some_and(|edit| edit.index == index) {
            return self.editor_element(index, cx);
        }
        let muted = theme::color(cx, "fg-muted");
        let chips: Vec<Div> = user
            .files
            .iter()
            .map(|file| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(6.0))
                    .rounded(theme::length(cx, "radius-small"))
                    .bg(theme::color(cx, "bg-hover"))
                    .text_size(theme::length(cx, "fs-small"))
                    .child(theme::icon(cx, theme::icons::file_icon_name(file), 12.0, muted))
                    .child(SharedString::from(file.clone()))
            })
            .collect();
        let images: Vec<Div> = user
            .images
            .iter()
            .enumerate()
            .map(|(n, image)| {
                let selector = format!("session-user-{index}-image-{n}");
                let name = image.name.clone().unwrap_or_else(|| tx(cx, "Image"));
                div()
                    .debug_selector(move || selector)
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .border_1()
                    .border_color(theme::color(cx, "border"))
                    .rounded(theme::length(cx, "radius-small"))
                    .child(theme::icon(cx, "file-image", 14.0, muted))
                    .child(SharedString::from(name))
            })
            .collect();
        let mut buttons = Vec::with_capacity(user.buttons.len());
        for button in &user.buttons {
            let (button, row) = (*button, user.clone());
            let selector = format!("session-user-{index}-{}", user_slug(button));
            buttons.push(self.button(selector, user_icon(button), true, cx, move |view, window, cx| {
                view.user_clicked(index, button, &row, window, cx);
            }));
        }
        div()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(4.0))
            .child(
                div()
                    .max_w(px(560.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(theme::length(cx, "radius-control"))
                    .bg(theme::color(cx, "bg-elev"))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(self.user_text(user, cx))
                    .when(!images.is_empty(), |el| el.child(div().flex().flex_wrap().gap(px(4.0)).children(images)))
                    .when(!chips.is_empty(), |el| el.child(div().flex().flex_wrap().gap(px(4.0)).children(chips))),
            )
            .child(self.footer(user.time.as_ref(), index, buttons, cx))
    }

    fn editor_element(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let Some(edit) = &self.editing else { return div() };
        let (sending, can_send, error) = (edit.editor.sending(), edit.editor.can_send(), edit.editor.error());
        let turn_count = {
            let store = self.store.read(cx);
            turn_numbers(store.entries(self.nav.task(), None).unwrap_or_default()).len()
        };
        let note = replaces_later(edit.turn, turn_count, &cx.default_global::<Localization>().0);
        let muted = theme::color(cx, "fg-muted");
        let mut images = Vec::new();
        for (n, image) in edit.editor.images().iter().enumerate() {
            let name = image.name.clone().unwrap_or_else(|| tx(cx, "Image"));
            images.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(6.0))
                    .border_1()
                    .border_color(theme::color(cx, "border"))
                    .rounded(theme::length(cx, "radius-small"))
                    .child(theme::icon(cx, "file-image", 14.0, muted))
                    .child(SharedString::from(name))
                    .child(self.button(
                        format!("session-edit-image-{n}-remove"),
                        "close",
                        !sending,
                        cx,
                        move |view, _, cx| view.remove_edit_image(n, cx),
                    )),
            );
        }
        let cancel = self.text_button(
            "session-edit-cancel".to_owned(),
            tx(cx, "Cancel"),
            ButtonVariant::Ghost,
            !sending,
            cx,
            |view, _, cx| view.close_editor(cx),
        );
        let send = self.text_button(
            "session-edit-send".to_owned(),
            tx(cx, "Send"),
            ButtonVariant::Primary,
            can_send,
            cx,
            |view, _, cx| view.send_edit(cx),
        );
        div()
            .debug_selector(move || format!("session-edit-{index}"))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .border_1()
            .border_color(theme::color(cx, "accent"))
            .rounded(theme::length(cx, "radius-control"))
            .child(div().h(theme::length(cx, "h-control")).child(edit.field.clone()))
            .when(!images.is_empty(), |el| el.child(div().flex().flex_wrap().gap(px(4.0)).children(images)))
            .children(note.map(|note| {
                div().text_color(muted).text_size(theme::length(cx, "fs-small")).child(SharedString::from(note))
            }))
            .children(
                error.map(|error| {
                    div().text_color(theme::color(cx, "danger")).child(SharedString::from(error.to_owned()))
                }),
            )
            .child(div().flex().justify_end().gap(px(6.0)).child(cancel).child(send))
    }

    fn agent_element(&self, index: usize, agent: &AgentRow, cwd: &str, cx: &mut Context<Self>) -> Div {
        let prefix = format!("session-md-{index}");
        let body = markdown_element(&agent.blocks, &prefix, cwd, &self.handler, cx);
        let mut buttons = Vec::with_capacity(agent.buttons.len());
        for button in &agent.buttons {
            let (button, text, turn) = (*button, agent.text.clone(), agent.turn);
            let selector = format!("session-reply-{index}-{}", reply_slug(button));
            buttons.push(self.button(selector, reply_icon(button, agent.copied), true, cx, move |view, _, cx| {
                view.reply_clicked(button, text.clone(), turn, cx);
            }));
        }
        div().flex().flex_col().gap(px(4.0)).child(body).child(self.footer(agent.time.as_ref(), index, buttons, cx))
    }

    fn fold_header(&self, selector: String, open: bool, fold: Fold, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = selector.clone();
        let muted = theme::color(cx, "fg-muted");
        div()
            .id(SharedString::from(id))
            .debug_selector(move || selector)
            .flex()
            .items_center()
            .gap(px(4.0))
            .text_color(muted)
            .rounded(theme::length(cx, "radius-small"))
            .hover(|el| el.bg(theme::color(cx, "bg-hover")))
            .child(theme::icon(cx, if open { "chevron-down" } else { "chevron-right" }, 14.0, muted))
            .on_click(cx.listener(move |view, _, _, cx| view.toggle_fold(fold, cx)))
    }

    fn row_body(&self, row: &RowModel, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let index = row.index;
        let muted = theme::color(cx, "fg-muted");
        match &row.kind {
            RowKind::User(user) => self.user_element(index, user, cx).into_any_element(),
            RowKind::Agent(agent) => self.agent_element(index, agent, &snapshot.cwd, cx).into_any_element(),
            RowKind::Thought { label, text, open } => {
                let header = self
                    .fold_header(format!("session-thought-{index}"), *open, Fold::Thought(index), cx)
                    .child(SharedString::from(label.clone()));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(header)
                    .children(
                        text.clone().map(|text| div().pl(px(18.0)).text_color(muted).child(SharedString::from(text))),
                    )
                    .into_any_element()
            }
            RowKind::Tool(tool) => {
                let (icon, color) = tool_icon(tool.status);
                let line = div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(theme::icon(cx, icon, 13.0, theme::color(cx, color)))
                    .child(div().flex_1().min_w_0().child(SharedString::from(tool.title.clone())))
                    .when(tool.added > 0, |el| el.child(counted(cx, "vcs-added", '+', tool.added)))
                    .when(tool.removed > 0, |el| el.child(counted(cx, "danger", '−', tool.removed)));
                let head = if tool.has_output {
                    let selector = format!("session-tool-{index}");
                    self.fold_header(selector, tool.open, Fold::Tool(index), cx).child(line).into_any_element()
                } else {
                    div().pl(px(18.0)).child(line).into_any_element()
                };
                let output = tool.output.clone().map(|output| {
                    let run = TextRun {
                        len: output.len().max(1),
                        font: mono_font(cx),
                        color: theme::color(cx, "fg"),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let output = if output.is_empty() { " ".to_owned() } else { output };
                    div()
                        .id(SharedString::from(format!("session-tool-output-{index}")))
                        .ml(px(18.0))
                        .px(px(8.0))
                        .py(px(4.0))
                        .max_h(px(240.0))
                        .overflow_y_scroll()
                        .rounded(theme::length(cx, "radius-small"))
                        .bg(theme::color(cx, "bg-editor"))
                        .text_size(theme::length(cx, "fs-code"))
                        .child(StyledText::new(output).with_runs(vec![run]))
                });
                div().flex().flex_col().gap(px(2.0)).child(head).children(output).into_any_element()
            }
            RowKind::Process { summary, open, children } => {
                let mut header = self
                    .fold_header(format!("session-fold-{index}"), *open, Fold::Process(index), cx)
                    .child(SharedString::from(summary.label.clone()));
                if summary.added > 0 {
                    header = header.child(counted(cx, "vcs-added", '+', summary.added));
                }
                if summary.removed > 0 {
                    header = header.child(counted(cx, "danger", '−', summary.removed));
                }
                if summary.working {
                    header = header.child(theme::icon(cx, "refresh", 12.0, muted));
                }
                let mut inner = Vec::with_capacity(children.len());
                for child in children {
                    inner.push(self.row_body(child, snapshot, cx));
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(header)
                    .when(!inner.is_empty(), |el| {
                        el.child(div().pl(px(18.0)).flex().flex_col().gap(px(4.0)).children(inner))
                    })
                    .into_any_element()
            }
            RowKind::Plan(items) => {
                let lines: Vec<Div> = items
                    .iter()
                    .map(|(content, status)| {
                        let (icon, color) = match status.as_str() {
                            "completed" => ("check", "success"),
                            "in_progress" => ("refresh", "accent"),
                            _ => ("minus", "fg-faint"),
                        };
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(theme::icon(cx, icon, 13.0, theme::color(cx, color)))
                            .child(SharedString::from(content.clone()))
                    })
                    .collect();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_color(muted).child(SharedString::from(tx(cx, "Plan"))))
                    .children(lines)
                    .into_any_element()
            }
            RowKind::Subagent { id, title, status, activity } => {
                let selector = format!("session-subagent-{id}");
                let open = id.clone();
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector)
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_1()
                    .border_color(theme::color(cx, "border"))
                    .rounded(theme::length(cx, "radius-control"))
                    .hover(|el| el.bg(theme::color(cx, "bg-hover")))
                    .child(theme::icon(cx, "toolwindow-agents", 14.0, muted))
                    .child(div().flex_1().min_w_0().child(SharedString::from(title.clone())))
                    .children(activity.clone().map(|text| div().text_color(muted).child(SharedString::from(text))))
                    .child(div().text_color(muted).child(SharedString::from(status.clone())))
                    .on_click(cx.listener(move |view, _, _, cx| view.open_subagent(&open, cx)))
                    .into_any_element()
            }
            RowKind::Message { incoming, blocks, .. } => {
                let label = tx(cx, if *incoming { "Task" } else { "Report" });
                let prefix = format!("session-md-{index}");
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .p(px(8.0))
                    .border_1()
                    .border_color(theme::color(cx, "border"))
                    .rounded(theme::length(cx, "radius-control"))
                    .child(small(cx, muted, label))
                    .child(markdown_element(blocks, &prefix, &snapshot.cwd, &self.handler, cx))
                    .into_any_element()
            }
            RowKind::Permission { id, line } => self.permission_element(*id, line, cx).into_any_element(),
            RowKind::Error(message) => {
                let danger = theme::color(cx, "danger");
                div().text_color(danger).child(SharedString::from(message.clone())).into_any_element()
            }
        }
    }

    fn permission_element(&self, id: u32, line: &PermissionLine, cx: &mut Context<Self>) -> Div {
        let muted = theme::color(cx, "fg-muted");
        if let Some(record) = &line.record {
            let (icon, color) = if record.allowed { ("check", "success") } else { ("close", "danger") };
            return div()
                .debug_selector(move || format!("session-permission-{id}"))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_color(muted)
                .child(theme::icon(cx, icon, 13.0, theme::color(cx, color)))
                .child(SharedString::from(format!("{}: {}", record.label, record.subject)));
        }
        let busy = self.responding.busy();
        let mut answers = Vec::with_capacity(line.answers.len());
        for (n, answer) in line.answers.iter().enumerate() {
            let option = answer.option.clone();
            answers.push(self.text_button(
                format!("session-permission-{id}-{n}"),
                answer.label.clone(),
                answer.variant,
                !busy,
                cx,
                move |view, _, cx| view.permission_clicked(id, option.clone(), cx),
            ));
        }
        let command = line.command.clone().map(|command| {
            let run = TextRun {
                len: command.len(),
                font: mono_font(cx),
                color: theme::color(cx, "fg"),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            div()
                .px(px(8.0))
                .py(px(4.0))
                .rounded(theme::length(cx, "radius-small"))
                .bg(theme::color(cx, "bg-editor"))
                .text_size(theme::length(cx, "fs-code"))
                .child(StyledText::new(command).with_runs(vec![run]))
        });
        let detail = (line.command.is_none() && !line.detail.is_empty())
            .then(|| div().text_color(muted).child(SharedString::from(line.detail.clone())));
        div()
            .debug_selector(move || format!("session-permission-{id}"))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .border_1()
            .border_color(theme::color(cx, "border-feedback-warning"))
            .bg(theme::color(cx, "bg-feedback-warning"))
            .rounded(theme::length(cx, "radius-control"))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(line.title.clone())))
            .children(command)
            .children(detail)
            .when(!answers.is_empty(), |el| el.child(div().flex().flex_wrap().gap(px(6.0)).children(answers)))
            .children(
                self.responding.error().map(|error| {
                    div().text_color(theme::color(cx, "danger")).child(SharedString::from(error.to_owned()))
                }),
            )
    }

    fn question_element(&self, cx: &mut Context<Self>) -> Option<Div> {
        let question = self.question.as_ref()?;
        let english = I18n::default();
        let muted = theme::color(cx, "fg-muted");
        let busy = question.form.busy();
        let mut blocks = Vec::new();
        for (position, q) in question.form.questions().iter().enumerate() {
            let i18n = cx.try_global::<Localization>().map_or(&english, |localization| &localization.0);
            let choices = question.form.choices(position, i18n);
            let preview = question.form.preview(position, i18n);
            let mut rows = Vec::with_capacity(choices.len());
            for choice in choices {
                let selector = format!("session-question-{position}-{}", choice.key);
                let (value, checked) = (choice.choice.clone(), choice.checked);
                let mark = if q.multi_select {
                    if checked { "check" } else { "minus" }
                } else if checked {
                    "check"
                } else {
                    "chevron-right"
                };
                let mark_color = theme::color(cx, if checked { "accent" } else { "fg-faint" });
                let number = format!("{}.", choice.key);
                let id = selector.clone();
                rows.push(
                    div()
                        .id(SharedString::from(id))
                        .debug_selector(move || selector)
                        .flex()
                        .items_start()
                        .gap(px(6.0))
                        .px(px(6.0))
                        .py(px(3.0))
                        .rounded(theme::length(cx, "radius-small"))
                        .when(checked, |el| el.bg(theme::color(cx, "bg-selected")))
                        .hover(|el| el.bg(theme::color(cx, "bg-hover")))
                        .child(div().flex_none().text_color(muted).child(SharedString::from(number)))
                        .child(theme::icon(cx, mark, 13.0, mark_color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().flex().gap(px(6.0)).child(SharedString::from(choice.label.clone())).when(
                                    choice.recommended,
                                    |el| {
                                        let (accent, tag) = (theme::color(cx, "accent"), tx(cx, "Recommended"));
                                        el.child(div().text_color(accent).child(SharedString::from(tag)))
                                    },
                                ))
                                .when(!choice.description.is_empty(), |el| {
                                    let description = SharedString::from(choice.description.clone());
                                    el.child(div().text_color(muted).child(description))
                                }),
                        )
                        .when(!busy, |el| {
                            el.on_click(cx.listener(move |view, _, window, cx| {
                                view.choice_clicked(position, value.clone(), checked, window, cx);
                            }))
                        }),
                );
            }
            let other = question
                .fields
                .get(position)
                .filter(|_| question.form.other(position).is_some())
                .cloned()
                .map(|field| div().ml(px(24.0)).h(theme::length(cx, "h-control")).child(field));
            let preview = preview.map(|preview| {
                div()
                    .p(px(6.0))
                    .rounded(theme::length(cx, "radius-small"))
                    .bg(theme::color(cx, "bg-editor"))
                    .child(div().text_color(muted).child(SharedString::from(preview.caption)))
                    .child(SharedString::from(preview.body.unwrap_or_else(|| tx(cx, "No preview"))))
            });
            blocks.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(small(cx, muted, q.header.clone()))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(q.question.clone())))
                    .children(rows)
                    .children(other)
                    .children(preview),
            );
        }
        let skip = self.text_button(
            "session-question-skip".to_owned(),
            tx(cx, "Skip"),
            ButtonVariant::Ghost,
            !busy,
            cx,
            |view, _, cx| view.question_skip(cx),
        );
        let submit = self.text_button(
            "session-question-submit".to_owned(),
            tx(cx, "Submit"),
            ButtonVariant::Primary,
            question.form.can_submit(),
            cx,
            |view, _, cx| view.question_submit(cx),
        );
        Some(
            div()
                .debug_selector(|| "session-question".to_owned())
                .track_focus(&question.focus)
                .on_key_down(cx.listener(Self::question_key))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .m(px(8.0))
                .p(px(10.0))
                .border_1()
                .border_color(theme::color(cx, "border-strong"))
                .rounded(theme::length(cx, "radius-control"))
                .bg(theme::color(cx, "bg-elev"))
                .children(blocks)
                .child(div().flex().justify_end().gap(px(6.0)).child(skip).child(submit)),
        )
    }

    fn header(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Option<Div> {
        let crumbs = snapshot.crumbs.len() > 1;
        if !crumbs && snapshot.meters.is_empty() {
            return None;
        }
        let muted = theme::color(cx, "fg-muted");
        let mut path = Vec::new();
        if crumbs {
            let depth = snapshot.crumbs.len() - 2;
            let back = self.button("session-back".to_owned(), "arrow-left", true, cx, move |view, _, cx| {
                view.navigate(depth, cx);
            });
            path.push(back.into_any_element());
            for (depth, title) in snapshot.crumbs.iter().enumerate() {
                let last = depth + 1 == snapshot.crumbs.len();
                let selector = format!("session-crumb-{depth}");
                let crumb = div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector)
                    .text_color(if last { theme::color(cx, "fg") } else { muted })
                    .child(SharedString::from(title.clone()))
                    .when(!last, |el| el.on_click(cx.listener(move |view, _, _, cx| view.navigate(depth, cx))));
                if depth > 0 {
                    path.push(div().text_color(muted).child("›").into_any_element());
                }
                path.push(crumb.into_any_element());
            }
        }
        let meters: Vec<Div> = snapshot
            .meters
            .iter()
            .map(|meter| div().text_color(muted).child(SharedString::from(meter.clone())))
            .collect();
        Some(
            div()
                .h(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(8.0))
                .border_b_1()
                .border_color(theme::color(cx, "border"))
                .text_size(theme::length(cx, "fs-small"))
                .child(div().flex().items_center().gap(px(4.0)).min_w_0().children(path))
                .child(div().debug_selector(|| "session-meters".to_owned()).flex().gap(px(8.0)).children(meters)),
        )
    }
}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SessionView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_question(cx);
        let mut snapshot = self.snapshot(cx);
        self.sync_scroll(snapshot.signature, cx);
        // The scroll just fed to `Follow` can stop or resume following in this frame.
        snapshot.jump = self.shows_jump_to_latest(cx);
        self.painted = snapshot.rows.iter().map(|row| row.index).collect();
        let icon = snapshot.task.as_ref().map(|t| provider_icon(&t.provider)).unwrap_or_default();
        let mut rows = Vec::with_capacity(snapshot.rows.len());
        for row in &snapshot.rows {
            let body = self.row_body(row, &snapshot, cx);
            let selector = format!("session-row-{}", row.index);
            rows.push(
                div()
                    .debug_selector(move || selector)
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .px(px(12.0))
                    .py(px(4.0))
                    .when(row.header, |el| el.child(self.author(&snapshot.provider, &icon, cx)))
                    .child(body),
            );
        }
        let scroll = div()
            .id("session-scroll")
            .debug_selector(|| "session-scroll".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll(cx.listener(|view, _, _, cx| {
                view.follow.user_input((view.clock)());
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|view, _, _, _| view.follow.pointer_down((view.clock)())))
            .on_mouse_up(MouseButton::Left, cx.listener(|view, _, _, cx| view.pointer_up(cx)))
            .children(rows);
        let jump = snapshot.jump.then(|| {
            div()
                .id("session-jump-to-latest")
                .debug_selector(|| "session-jump-to-latest".to_owned())
                .absolute()
                .bottom(px(12.0))
                .right(px(16.0))
                .size(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(theme::color(cx, "border"))
                .bg(theme::color(cx, "bg-popup"))
                .hover(|el| el.bg(theme::color(cx, "bg-hover")))
                .child(theme::icon(cx, "arrow-down", 16.0, theme::color(cx, "fg")))
                .on_click(cx.listener(|view, _, _, cx| view.jump_to_latest(cx)))
        });
        let composer = self.composer.clone().filter(|_| self.nav.shows_composer());
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .text_size(theme::length(cx, "fs-ui"))
            .text_color(theme::color(cx, "fg"))
            .bg(theme::color(cx, "bg-editor"))
            .children(self.header(&snapshot, cx))
            .child(div().relative().flex_1().min_h_0().child(scroll).children(jump))
            .children(self.question_element(cx))
            .children(composer.map(|composer| div().flex_none().child(composer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::fixtures::{
        agent, calls, connected_store, local_task, permission, permission_option, question, subagent, thought, tool,
        tool_with, user,
    };
    use crate::agents::store::AgentCall;
    use gpui_kit::{Bounds, Modifiers, Pixels, ScrollDelta, ScrollEvent, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    mod proofs8_agent_reply;

    type Events = Rc<RefCell<Vec<SessionEvent>>>;

    fn clock() -> f64 {
        1_000_000.0
    }

    /// A ready task in the local checkout that can drop turns and be copied.
    fn editable(id: &str) -> AgentTask {
        let mut task = local_task(id);
        task.capabilities.rollback = true;
        task.capabilities.fork = true;
        task
    }

    /// The conversation of the first of `tasks`, holding `entries`, in a window of its own.
    fn open(
        cx: &mut TestAppContext,
        tasks: Vec<AgentTask>,
        entries: Vec<AgentEntry>,
    ) -> (Entity<SessionView>, Entity<AgentsStore>, Events, &mut VisualTestContext) {
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            // Once per app: a second init would run every key claim twice.
            if !cx.has_global::<input::Keymap>() {
                input::init(cx);
            }
        });
        let id = tasks[0].id.clone();
        let (mut store, _) = connected_store(tasks);
        store.entries_loaded(&id, None, entries);
        let store = cx.new(|_| store);
        let shared = store.clone();
        let (view, cx) = cx.add_window_view(move |_, cx| SessionView::new(shared, &id, false, cx).with_clock(clock));
        let events: Events = Rc::default();
        let seen = Rc::clone(&events);
        cx.update(|_, cx| {
            cx.subscribe(&view, move |_, event: &SessionEvent, _| seen.borrow_mut().push(event.clone())).detach();
        });
        cx.run_until_parked();
        (view, store, events, cx)
    }

    fn lines(view: &Entity<SessionView>, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |view, cx| view.lines(cx))
    }

    fn buttons(view: &Entity<SessionView>, index: usize, cx: &mut VisualTestContext) -> Vec<String> {
        view.update(cx, |view, cx| view.buttons(index, cx))
    }

    fn bounds_of(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn"))
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let centre = bounds_of(cx, selector).center();
        cx.simulate_click(centre, Modifiers::default());
        cx.run_until_parked();
    }

    fn effect_calls(events: &Events) -> Vec<AgentCall> {
        events
            .borrow()
            .iter()
            .flat_map(|event| match event {
                SessionEvent::Effects(effects) => calls(effects),
                _ => Vec::new(),
            })
            .collect()
    }

    fn messages(events: &Events) -> Vec<MessageAction> {
        events
            .borrow()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Message(action) => Some(action.clone()),
                _ => None,
            })
            .collect()
    }

    /// The ticket of the last call the view asked for.
    fn last_ticket(events: &Events) -> Ticket {
        events
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::Effects(effects) => effects.iter().find_map(|effect| match effect {
                    AgentEffect::Call { ticket: Some(ticket), .. } => Some(*ticket),
                    _ => None,
                }),
                _ => None,
            })
            .expect("a call with a ticket")
    }

    fn user_at(text: &str, sent_ms: f64, done_ms: f64) -> AgentEntry {
        AgentEntry::User {
            text: text.to_owned(),
            attachments: Vec::new(),
            images: Vec::new(),
            sent_ms: Some(sent_ms),
            done_ms: Some(done_ms),
            steered: false,
        }
    }

    // ----- conversation rows -----

    #[gpui_kit::test]
    async fn keeps_authors_above_messages_and_their_folded_work_with_matching_icon_only_actions(
        cx: &mut TestAppContext,
    ) {
        let entries = vec![
            user("First question", false),
            agent("First reply"),
            tool("read"),
            agent("More detail"),
            user("Next question", false),
            thought("Considering the next question"),
            tool("next"),
            agent("Next reply"),
        ];
        let (view, _, _, cx) = open(cx, vec![local_task("A")], entries);
        assert_eq!(
            lines(&view, cx),
            [
                "you: First question | 1ms",
                "author: codex",
                "agent: First reply | 1ms",
                "tool: read",
                "agent: More detail | 1ms",
                "you: Next question | 1ms",
                "author: codex",
                "fold: Reasoning · 1 tool call",
                "agent: Next reply | 1ms",
            ]
        );
        for reply in [1, 3, 7] {
            let labels = buttons(&view, reply, cx);
            assert!(labels.iter().any(|l| l == "Quote Reply") && labels.iter().any(|l| l == "Copy Markdown"));
        }
        assert_eq!(buttons(&view, 0, cx).first().map(String::as_str), Some("Copy"));
    }

    #[gpui_kit::test]
    async fn starts_another_author_run_after_subagent_entries(cx: &mut TestAppContext) {
        let entries = vec![
            agent("Before the switch"),
            subagent("c1", "Explore"),
            thought("After the switch"),
            tool("read"),
            agent("After the work"),
        ];
        let (view, _, _, cx) = open(cx, vec![local_task("A")], entries);
        assert_eq!(
            lines(&view, cx),
            [
                "author: codex",
                "agent: Before the switch",
                "subagent: Explore (Done)",
                "author: codex",
                "fold: Reasoning · 1 tool call",
                "agent: After the work",
            ]
        );
    }

    #[gpui_kit::test]
    async fn anchors_every_agent_footer_with_its_time_before_the_hover_actions(cx: &mut TestAppContext) {
        let entries =
            vec![user_at("Question", 1000.0, 13000.0), agent("First reply"), tool("read"), agent("Final reply")];
        let (view, _, _, cx) = open(cx, vec![local_task("A")], entries);
        let shown = lines(&view, cx);
        assert_eq!(shown.iter().filter(|line| line.starts_with("agent:") && line.ends_with("| 12s")).count(), 2);
        for (time, copy) in [("session-time-1", "session-reply-1-copy"), ("session-time-3", "session-reply-3-copy")] {
            assert!(bounds_of(cx, time).right() <= bounds_of(cx, copy).left(), "{time} comes before {copy}");
        }
        // The message the user sent has its footer too.
        assert!(cx.debug_bounds("session-time-0").is_some());
    }

    // ----- folds -----

    #[gpui_kit::test]
    async fn folds_reasoning_and_tool_calls_into_one_block(cx: &mut TestAppContext) {
        let entries = vec![thought("think one"), tool("one"), thought("think two"), tool("two"), agent("Answer A")];
        let (view, _, _, cx) = open(cx, vec![local_task("A")], entries);
        assert_eq!(lines(&view, cx), ["author: codex", "fold: Reasoning · 2 tool calls", "agent: Answer A"]);
        // Folded content is rendered once opened, a level at a time.
        click(cx, "session-fold-0");
        assert_eq!(
            lines(&view, cx),
            [
                "author: codex",
                "fold: Reasoning · 2 tool calls (open)",
                "  thought: Reasoning",
                "  tool: one",
                "  thought: Reasoning",
                "  tool: two",
                "agent: Answer A",
            ]
        );
        click(cx, "session-thought-0");
        assert!(lines(&view, cx).contains(&"  thought: Reasoning | think one".to_owned()));
        click(cx, "session-fold-0");
        assert_eq!(lines(&view, cx)[1], "fold: Reasoning · 2 tool calls");
    }

    #[gpui_kit::test]
    async fn is_rendered_for_find_in_conversation_so_its_text_is_found(cx: &mut TestAppContext) {
        let entries = vec![
            tool_with("one", AgentToolStatus::Completed, Some("needle in a tool output")),
            tool("two"),
            agent("Answer A"),
        ];
        let (view, _, _, cx) = open(cx, vec![local_task("A")], entries);
        assert!(!lines(&view, cx).iter().any(|line| line.contains("needle")));
        view.update(cx, |view, cx| view.set_finding(true, cx));
        cx.run_until_parked();
        let shown = lines(&view, cx);
        assert!(shown.contains(&"  tool: one | needle in a tool output".to_owned()), "{shown:?}");
        assert!(shown.contains(&"fold: 2 tool calls".to_owned()), "the fold stays closed: {shown:?}");
        assert!(view.read_with(cx, |view, cx| view.rendered_texts(cx)).iter().any(|t| t == "needle in a tool output"));
    }

    #[gpui_kit::test]
    async fn keeps_status_tools_outside_folds_with_their_output_expanded(cx: &mut TestAppContext) {
        for status in [AgentToolStatus::Pending, AgentToolStatus::Running] {
            let entries = vec![agent("Looking"), tool_with("List .", status, Some("src")), agent("Done")];
            let (view, _, _, cx) = open(cx, vec![local_task("A")], entries);
            assert_eq!(lines(&view, cx), ["author: codex", "agent: Looking", "tool: List . | src", "agent: Done"]);
        }
    }

    #[gpui_kit::test]
    async fn folds_consecutive_finished_tool_calls_into_one_collapsed_row(cx: &mut TestAppContext) {
        let (view, _, _, cx) = open(cx, vec![local_task("A")], vec![tool("a"), tool("b"), agent("Done")]);
        assert_eq!(lines(&view, cx), ["author: codex", "fold: 2 tool calls", "agent: Done"]);
    }

    // ----- permission requests and questions -----

    #[gpui_kit::test]
    async fn is_answered_from_its_line_too_and_one_without_options_can_be_cancelled(cx: &mut TestAppContext) {
        let options =
            vec![permission_option("allow", "Allow", "allow_once"), permission_option("deny", "Reject", "reject_once")];
        let entries =
            vec![agent("Checking"), permission(1, "Run python3 -V", options), permission(2, "Run ls", Vec::new())];
        let (view, store, events, cx) = open(cx, vec![local_task("A")], entries);
        let shown = lines(&view, cx);
        assert!(shown[2].starts_with("permission: ") && shown[2].contains("python3 -V"), "{shown:?}");
        assert!(shown[3].ends_with("[Cancel]"), "{shown:?}");
        click(cx, "session-permission-1-0");
        let respond = |permission, option: Option<&str>| AgentCall::Respond {
            task: "A".to_owned(),
            permission,
            option: option.map(str::to_owned),
        };
        assert_eq!(effect_calls(&events), [respond(1, Some("allow"))]);
        // One answer at a time.
        click(cx, "session-permission-2-0");
        assert_eq!(effect_calls(&events).len(), 1);
        let ticket = last_ticket(&events);
        view.update(cx, |view, cx| view.answered(ticket, Ok(()), cx));
        cx.run_until_parked();
        click(cx, "session-permission-2-0");
        assert_eq!(effect_calls(&events), [respond(1, Some("allow")), respond(2, None)]);
        // Once the agent reports the choice, the line keeps only its record.
        let answered = AgentEntry::Permission {
            id: 1,
            title: "Run python3 -V".to_owned(),
            options: vec![permission_option("allow", "Allow", "allow_once")],
            questions: Vec::new(),
            chosen: Some("Allow".to_owned()),
        };
        store.update(cx, |store, cx| {
            store.entries_loaded("A", None, vec![agent("Checking"), answered]);
            cx.notify();
        });
        cx.run_until_parked();
        let shown = lines(&view, cx);
        assert!(shown[2].starts_with("permission: ") && !shown[2].contains('['), "{shown:?}");
    }

    fn ask(id: u32) -> AgentEntry {
        AgentEntry::Permission {
            id,
            title: "Questions".to_owned(),
            options: vec![permission_option("reject", "Reject", "reject_once")],
            questions: vec![question(
                "Lang",
                "Which language?",
                false,
                &[("Rust (Recommended)", "Fast", None), ("Go", "Simple", None)],
            )],
            chosen: None,
        }
    }

    #[gpui_kit::test]
    async fn shows_every_question_with_its_descriptions_in_a_card_and_sends_all_answers_at_once(
        cx: &mut TestAppContext,
    ) {
        let (view, _, events, cx) = open(cx, vec![local_task("A")], vec![agent("Let me ask"), ask(3)]);
        let card = view.read_with(cx, |view, cx| view.card(cx));
        assert_eq!(
            card.as_deref(),
            Some(
                ["Lang: Which language?", "( ) Rust [Recommended]", "( ) Go", "( ) Other"]
                    .map(str::to_owned)
                    .as_slice()
            )
        );
        assert!(!view.read_with(cx, |view, _| view.can_submit()));
        click(cx, "session-question-0-2");
        assert!(view.read_with(cx, |view, cx| view.card(cx)).is_some_and(|card| card[2] == "(x) Go"));
        assert!(view.read_with(cx, |view, _| view.can_submit()));
        click(cx, "session-question-submit");
        let answer = serde_json::json!({ "Which language?": ["Go"] }).to_string();
        assert_eq!(
            effect_calls(&events),
            [AgentCall::Respond { task: "A".to_owned(), permission: 3, option: Some(answer) }]
        );
        assert!(!view.read_with(cx, |view, _| view.can_submit()), "the fields wait for the answer");
    }

    #[gpui_kit::test]
    async fn picks_options_with_number_keys_and_skips_with_escape(cx: &mut TestAppContext) {
        let (view, _, events, cx) = open(cx, vec![local_task("A")], vec![ask(4)]);
        // A click focuses the card; number keys answer the question from there.
        click(cx, "session-question-0-2");
        cx.simulate_keystrokes("1");
        cx.run_until_parked();
        let card = view.read_with(cx, |view, cx| view.card(cx)).expect("a card");
        assert_eq!((card[1].as_str(), card[2].as_str()), ("(x) Rust [Recommended]", "( ) Go"));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(
            effect_calls(&events),
            [AgentCall::Respond { task: "A".to_owned(), permission: 4, option: Some("reject".to_owned()) }]
        );
    }

    // ----- scrolling -----

    fn long_conversation(n: usize) -> Vec<AgentEntry> {
        (0..n).map(|i| agent(&format!("Reply {i}\n\nwith a second paragraph\n\nand a third"))).collect()
    }

    fn scroll_up(cx: &mut VisualTestContext, by: f32) {
        let centre = bounds_of(cx, "session-scroll").center();
        cx.simulate_event(ScrollEvent {
            position: centre,
            delta: ScrollDelta::Pixels(point(px(0.0), px(by))),
            ..Default::default()
        });
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    async fn offers_one_click_back_to_the_latest_message_after_scrolling_up(cx: &mut TestAppContext) {
        let (view, _, _, cx) = open(cx, vec![local_task("A")], long_conversation(60));
        assert!(view.read_with(cx, |view, _| view.follows()));
        assert!(!view.read_with(cx, |view, cx| view.shows_jump_to_latest(cx)));
        assert!(cx.debug_bounds("session-jump-to-latest").is_none());
        scroll_up(cx, 400.0);
        assert!(!view.read_with(cx, |view, _| view.follows()));
        assert!(view.read_with(cx, |view, cx| view.shows_jump_to_latest(cx)));
        click(cx, "session-jump-to-latest");
        assert!(view.read_with(cx, |view, _| view.follows()));
        assert!(!view.read_with(cx, |view, cx| view.shows_jump_to_latest(cx)));
        assert!(cx.debug_bounds("session-jump-to-latest").is_none());
    }

    #[gpui_kit::test]
    async fn hides_jump_to_latest_and_follows_new_replies_after_retracting_the_conversation(cx: &mut TestAppContext) {
        let (view, store, _, cx) = open(cx, vec![local_task("A")], long_conversation(60));
        scroll_up(cx, 400.0);
        assert!(view.read_with(cx, |view, cx| view.shows_jump_to_latest(cx)));
        store.update(cx, |store, cx| {
            store.entries_loaded("A", None, vec![user("Question", false)]);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!view.read_with(cx, |view, cx| view.shows_jump_to_latest(cx)));
        assert!(view.read_with(cx, |view, _| view.follows()));
        store.update(cx, |store, cx| {
            store.entries_loaded("A", None, long_conversation(60));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.follows()));
        assert!(!view.read_with(cx, |view, cx| view.shows_jump_to_latest(cx)));
    }

    #[gpui_kit::test]
    async fn opens_a_long_conversation_at_its_end_and_renders_the_earlier_entries_after_it(cx: &mut TestAppContext) {
        let (view, _, _, cx) = open(cx, vec![local_task("A")], long_conversation(100));
        assert!(view.read_with(cx, |view, _| view.follows()));
        assert_eq!(view.read_with(cx, |view, _| view.first_painted()), Some(0));
        assert!(cx.debug_bounds("session-row-99").is_some());
    }

    // ----- subagents -----

    #[gpui_kit::test]
    async fn opens_a_subagent_conversation_and_goes_back(cx: &mut TestAppContext) {
        let (view, _, events, cx) = open(cx, vec![local_task("A")], vec![agent("Starting"), subagent("c1", "Explore")]);
        click(cx, "session-subagent-c1");
        assert_eq!(view.read_with(cx, |view, _| view.path().to_vec()), ["c1"]);
        let loads: Vec<(String, Option<String>)> = events
            .borrow()
            .iter()
            .flat_map(|event| match event {
                SessionEvent::Effects(effects) => effects
                    .iter()
                    .filter_map(|effect| match effect {
                        AgentEffect::LoadEntries { task, subagent } => Some((task.clone(), subagent.clone())),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(loads, [("A".to_owned(), Some("c1".to_owned()))]);
        click(cx, "session-back");
        assert!(view.read_with(cx, |view, _| view.path().is_empty()));
    }

    #[gpui_kit::test]
    async fn returns_to_the_task_conversation_when_the_docked_task_changes_after_opening_a_subagent(
        cx: &mut TestAppContext,
    ) {
        let (view, _, _, cx) =
            open(cx, vec![local_task("A"), local_task("B")], vec![agent("Starting"), subagent("c1", "Explore")]);
        click(cx, "session-subagent-c1");
        view.update(cx, |view, cx| view.show_task("B", cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.task().to_owned()), "B");
        assert!(view.read_with(cx, |view, _| view.path().is_empty()));
    }

    // ----- message actions -----

    #[gpui_kit::test]
    async fn copies_the_exact_markdown_source_and_briefly_confirms_success(cx: &mut TestAppContext) {
        let source = "**Done**: see `main.rs`";
        let (view, _, _, cx) = open(cx, vec![local_task("A")], vec![user("Question", false), agent(source)]);
        click(cx, "session-reply-1-copy");
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(source));
        assert_eq!(buttons(&view, 1, cx).first().map(String::as_str), Some("Copied"));
    }

    #[gpui_kit::test]
    async fn does_not_offer_copying_an_empty_reply(cx: &mut TestAppContext) {
        for text in ["", " \n\t"] {
            let (view, _, _, cx) = open(cx, vec![local_task("A")], vec![user("Question", false), agent(text)]);
            assert!(!buttons(&view, 1, cx).iter().any(|label| label == "Copy Markdown"), "{text:?}");
        }
    }

    #[gpui_kit::test]
    async fn quotes_and_regenerates_the_latest_reply(cx: &mut TestAppContext) {
        let (view, _, events, cx) = open(cx, vec![editable("A")], vec![user("Question", false), agent("Answer")]);
        assert_eq!(
            buttons(&view, 1, cx),
            ["Copy Markdown", "Quote Reply", "Regenerate Response", "Branch Conversation from Here"]
        );
        click(cx, "session-reply-1-quote");
        click(cx, "session-reply-1-regenerate");
        let target = ReplyTarget { task: "A".to_owned(), turn: 0 };
        assert_eq!(
            messages(&events),
            [
                MessageAction::Quote { task: "A".to_owned(), text: "Answer".to_owned() },
                MessageAction::Regenerate(target),
            ]
        );
    }

    // ----- editing a sent message -----

    fn start_editing(view: &Entity<SessionView>, cx: &mut VisualTestContext) {
        click(cx, "session-user-0-edit");
        assert_eq!(view.read_with(cx, |view, _| view.editing()), Some(0));
    }

    fn edit_text(view: &Entity<SessionView>, cx: &mut VisualTestContext) -> Option<String> {
        view.read_with(cx, |view, _| view.edit_text().map(str::to_owned))
    }

    #[gpui_kit::test]
    async fn names_the_icon_only_edit_and_resend_action_and_disables_empty_edits(cx: &mut TestAppContext) {
        let (view, _, _, cx) = open(cx, vec![editable("A")], vec![user("Original", false), agent("Reply")]);
        assert_eq!(
            buttons(&view, 0, cx),
            ["Copy", "Edit", "Retract Message", "Run Again From Here", "Copy to a New Task From Here"]
        );
        start_editing(&view, cx);
        assert_eq!(edit_text(&view, cx).as_deref(), Some("Original"));
        assert!(view.read_with(cx, |view, _| view.can_send_edit()));
        cx.simulate_keystrokes("cmd-a backspace");
        cx.run_until_parked();
        assert_eq!(edit_text(&view, cx).as_deref(), Some(""));
        assert!(!view.read_with(cx, |view, _| view.can_send_edit()));
    }

    #[gpui_kit::test]
    async fn keeps_enter_shift_enter_and_ime_confirmation_for_editing(cx: &mut TestAppContext) {
        let (view, _, events, cx) = open(cx, vec![editable("A")], vec![user("Original", false), agent("Reply")]);
        start_editing(&view, cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.simulate_keystrokes("shift-enter");
        cx.run_until_parked();
        assert_eq!(edit_text(&view, cx).as_deref(), Some("Original\n\n"));
        assert!(messages(&events).is_empty(), "a line break sends nothing");
    }

    #[gpui_kit::test]
    async fn still_sends_with_modified_enter(cx: &mut TestAppContext) {
        for keys in ["cmd-enter", "ctrl-enter"] {
            let (view, _, events, cx) = open(cx, vec![editable("A")], vec![user("Original", false), agent("Reply")]);
            start_editing(&view, cx);
            cx.simulate_input(" text");
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            let target = ReplyTarget { task: "A".to_owned(), turn: 0 };
            assert_eq!(
                messages(&events),
                [MessageAction::EditSend { target, text: "Original text".to_owned(), images: Vec::new() }],
                "{keys}"
            );
            view.update(cx, |view, cx| view.edit_sent(Ok(()), cx));
            assert_eq!(view.read_with(cx, |view, _| view.editing()), None);
        }
    }

    #[gpui_kit::test]
    async fn keeps_the_draft_and_shows_a_rollback_failure_beside_it(cx: &mut TestAppContext) {
        let (view, _, _, cx) = open(cx, vec![editable("A")], vec![user("Original", false), agent("Reply")]);
        start_editing(&view, cx);
        click(cx, "session-edit-send");
        view.update(cx, |view, cx| view.edit_sent(Err("rollback failed".to_owned()), cx));
        assert_eq!(view.read_with(cx, |view, _| view.editing()), Some(0));
        assert_eq!(edit_text(&view, cx).as_deref(), Some("Original"));
        assert!(view.read_with(cx, |view, _| view.can_send_edit()));
    }

    #[gpui_kit::test]
    async fn escape_and_cancel_restore_the_message_and_discard_edits_without_touching_the_conversation(
        cx: &mut TestAppContext,
    ) {
        let (view, _, events, cx) = open(cx, vec![editable("A")], vec![user("Original", false), agent("Reply")]);
        start_editing(&view, cx);
        cx.simulate_input(" changed");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.editing()), None);
        assert_eq!(lines(&view, cx)[0], "you: Original | 1ms");
        start_editing(&view, cx);
        assert_eq!(edit_text(&view, cx).as_deref(), Some("Original"));
        click(cx, "session-edit-cancel");
        assert_eq!(view.read_with(cx, |view, _| view.editing()), None);
        assert!(messages(&events).is_empty() && effect_calls(&events).is_empty());
    }

    #[test]
    fn line_breaks_are_marks_in_the_edit_field() {
        assert_eq!(marked("a\nb\r\nc"), "a⏎b⏎c");
        assert_eq!(unmarked("a⏎b"), "a\nb");
    }
}
