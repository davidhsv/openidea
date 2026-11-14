//! What running plugin code does in a window, shown and answered there.
//!
//! The window listens to the app's plugin host and carries out what the host announces for it (or
//! for every window): status bar items the code sets, updates and removes over the items the
//! plugins declare; background tasks with their progress and Cancel; notifications whose buttons
//! run the publishing plugin's command; context keys for `when` clauses; files opened at a
//! position; URLs opened in the browser; text copied to the clipboard; projects opened; the
//! questions a waiting command asks (text, pick one, pick several, yes / no, a folder); the log
//! tabs plugin code opens and the output channels it reveals; the host's requests to run an
//! action or a plugin command for it.
//!
//! The window also tells the plugins what happens in it: the focused editor's file
//! (`editor-focused`), files opened and closed, actions run, repositories and settings changed,
//! in batches at most every [`POST_INTERVAL`], and only the kinds some plugin wants. It sets the
//! `workspaceContains:<path>` keys the plugins' `when` clauses name from the listing of each
//! path's folder, again when the contributions, the project, or a watched path change.
//!
//! Every call to the host is first authorized for this window as its desktop command would be
//! (`Host::authorize_command` under the command's IPC name): calls that may change what plugin
//! code does run only in a trusted project. The host itself checks each plugin's grant.

use super::notifications::{NotificationAction, NotificationLevel, NotificationRequest};
use super::status_bar::{StatusAlign, StatusClick, StatusContent, StatusItemSpec, StatusText};
use super::tab_views::TabView;
use super::tasks::{Cancel, CancelEffect, Change, TaskId, TaskUpdate};
use super::{Workbench, for_window};
use crate::backend::{Backend, BackendEvent};
use crate::editor::tabs::EditorInput;
use crate::platform::actions::{Action, ActionMeta, ActionRegistry, ActionSource, execute_action_or_throw};
use crate::platform::context::{clear_context, set_context};
use crate::platform::i18n::{Localization, Param};
use crate::platform::input::Keymap;
use crate::platform::keymap::is_mac_host;
use crate::platform::local_time::now_ms;
use crate::plugin_models::active_file::ActiveFile;
use crate::plugin_models::contribution::Contribution;
use crate::plugin_models::runtime_ui::{PluginCommandCall, RuntimeEffect, RuntimeUi};
use crate::plugin_models::store::{command_failure, command_title};
use crate::plugin_models::workspace_contains::{ContainsRequest, WorkspaceContains};
use crate::plugin_views::log_tab::{
    LOG_COLUMNS, LOG_ROWS, LogOrigin, LogTabData, PLUGIN_LOG_ICON, PluginLogEvent, PluginLogTab, code_log_input,
    output_input,
};
use crate::plugin_views::prompt_dialog::{PluginPrompt, PluginPromptEvent};
use crate::plugins::{Plugins, plugin_action_id};
use futures::StreamExt as _;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, PathPromptOptions, Subscription, Task,
    WeakEntity, Window, div, prelude::*,
};
use ide_core::{IdeEvent, IdeResult, TerminalId};
use ide_host::plugins::{
    ActionPerformed, CommandAnswer, CommitAnswer, EventKind, PluginEditor, PluginEvent, PluginHost, PluginStatus,
    PluginUpdate, PluginsSnapshot, Prompt, StatusItem, WindowRequest, WorkbenchEvent,
};
use ide_plugin::Invocation;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Workbench events are sent at most this often; the host batches again per plugin.
const POST_INTERVAL: Duration = Duration::from_millis(500);
/// How long a window waits for the installed plugins to load before asking what events they want.
const INITIAL_LOAD_WAIT: Duration = Duration::from_secs(5);
/// The id prefix of plugin status bar items: `plugin.<plugin>/<item>`.
pub(super) const PLUGIN_STATUS_PREFIX: &str = "plugin.";
/// Declared items without a side or a priority sit on the right, priority 2.
const DEFAULT_PRIORITY: f64 = 2.0;
/// The extension point of declared status items.
const STATUS_POINT: &str = "statusBar";
/// The log tab actions besides Toggle Log Option; they are not in the generated action list.
const LOG_OPERATIONS: [(&str, &str); 2] = [("plugins.log.clear", "Clear"), ("plugins.log.wrap", "Soft-wrap")];
const TOGGLE_ARGUMENT: &str = "plugins.log.toggleArgument";
/// Exits of terminals not yet known to a tab, kept at most this many.
const MAX_EARLY_EXITS: usize = 32;

/// A status item a plugin declares (`[[contributes.statusBar]]`): shown until its code sets one.
#[derive(Clone, Debug, PartialEq)]
struct DeclaredItem {
    plugin_id: String,
    item: StatusItem,
    align: StatusAlign,
    priority: f64,
}

/// A question on screen and what it answers.
struct OpenPrompt {
    prompt_id: String,
    view: Entity<PluginPrompt>,
    /// What had the keyboard before the question came.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

/// The program of an open log tab.
struct LogSession {
    view: WeakEntity<PluginLogTab>,
    /// The terminal running the tab's program, once it started.
    terminal: Option<TerminalId>,
    /// Counts the starts: an answer for an earlier start is dropped.
    generation: u64,
    /// Forwards the terminal's output to the tab; dropping it stops that.
    forward: Option<Task<()>>,
    _released: Subscription,
}

/// What the window listens to while it lives.
struct Listeners {
    plugins: u64,
    backend: Option<u64>,
    actions: u64,
    _observers: Vec<Subscription>,
}

/// The plugin runtime of one window.
#[derive(Default)]
pub(super) struct PluginRuntime {
    listeners: Option<Listeners>,
    ui: RuntimeUi,
    contains: WorkspaceContains,
    active_file: ActiveFile,
    /// The project root last seen (`Some(None)`: none); `None` before the first look.
    root: Option<Option<String>>,
    /// The kinds of workbench events some plugin wants.
    wanted: BTreeSet<EventKind>,
    queue: Vec<WorkbenchEvent>,
    post_timer: Option<Task<()>>,
    /// Status listings: the latest request, and the changes counted since it was made.
    status_request: u64,
    status_revision: u64,
    declared: BTreeMap<String, DeclaredItem>,
    /// The plugin items registered with the status bar, by bar id.
    shown_items: BTreeMap<String, StatusItemSpec>,
    /// The files open in editor tabs, as `file-opened` / `file-closed` were reported.
    open_files: BTreeSet<String>,
    /// The questions on screen, the newest last (it is the one drawn).
    prompts: Vec<OpenPrompt>,
    /// Folder questions the system dialog shows; one the plugin gave up on is answered no more.
    folder_prompts: BTreeSet<String>,
    logs: HashMap<String, LogSession>,
    /// Exits of terminals whose start has not been answered yet: a program that fails at once can
    /// end before its tab learns which terminal it runs in.
    early_exits: HashMap<TerminalId, Option<i32>>,
    /// The output channels' last snapshots, by (plugin, channel).
    outputs: HashMap<(String, String), String>,
    /// What each plugin looked like at the last listing: a change stops its log tabs.
    plugin_states: HashMap<String, String>,
}

/// The app's plugin host, once it runs.
pub(super) fn plugin_host(cx: &App) -> Option<Arc<PluginHost>> {
    cx.try_global::<Plugins>().map(|plugins| plugins.host.clone())
}

/// The contributions of the live plugins in `listed`.
fn plugin_contributions(listed: Option<&PluginsSnapshot>) -> Vec<Contribution> {
    listed.map(Contribution::from_snapshot).unwrap_or_default()
}

/// The name of plugin `plugin_id` in `listed`, else its id.
fn plugin_name(plugin_id: &str, listed: Option<&PluginsSnapshot>) -> String {
    listed
        .and_then(|listed| listed.plugins.iter().find(|plugin| plugin.id == plugin_id))
        .map_or_else(|| plugin_id.to_owned(), |plugin| plugin.name.clone())
}

impl Workbench {
    /// The plugins this window sees, as last listed: the installed and linked ones, plus its
    /// project's own while that project is trusted.
    pub(super) fn plugin_listing<'a>(&self, cx: &'a App) -> Option<&'a PluginsSnapshot> {
        let project = cx.try_global::<Backend>().and_then(|backend| backend.host.project(&self.label).ok());
        cx.try_global::<Plugins>()?.listing_for(project.as_ref().map(|project| project.root.as_path()))
    }
}

/// The declared status items of a listing, by `<plugin>/<id>`.
fn declared_items(contributions: &[Contribution]) -> BTreeMap<String, DeclaredItem> {
    contributions
        .iter()
        .filter(|c| c.point == STATUS_POINT)
        .filter_map(|c| {
            let id = c.text("id")?;
            let item = StatusItem {
                text: c.text("text").unwrap_or_default().to_owned(),
                tooltip: c.text("tooltip").map(str::to_owned),
                command: c.text("command").map(str::to_owned),
            };
            let align = if c.text("align") == Some("left") { StatusAlign::Left } else { StatusAlign::Right };
            let priority = c.decl.get("priority").and_then(Value::as_f64).unwrap_or(DEFAULT_PRIORITY);
            let declared = DeclaredItem { plugin_id: c.plugin_id.clone(), item, align, priority };
            Some((format!("{}/{id}", c.plugin_id), declared))
        })
        .collect()
}

/// A context key's value as `when` clauses read it: `None` clears the key. Lists and objects are
/// kept as their JSON text.
fn context_value(value: &Value) -> Option<crate::platform::context::ContextValue> {
    use crate::platform::context::ContextValue;
    match value {
        Value::Null => None,
        Value::Bool(on) => Some(ContextValue::Bool(*on)),
        Value::Number(number) => number.as_f64().map(ContextValue::Number),
        Value::String(text) => Some(ContextValue::from(text.as_str())),
        other => Some(ContextValue::from(other.to_string())),
    }
}

/// The source of an action as plugins learn it.
fn plugin_source(source: ActionSource) -> ide_plugin::ActionSource {
    match source {
        ActionSource::Keyboard => ide_plugin::ActionSource::Keyboard,
        ActionSource::Menu => ide_plugin::ActionSource::Menu,
        ActionSource::Toolbar => ide_plugin::ActionSource::Toolbar,
        ActionSource::ContextMenu => ide_plugin::ActionSource::ContextMenu,
        ActionSource::Search => ide_plugin::ActionSource::Search,
        ActionSource::Other => ide_plugin::ActionSource::Other,
    }
}

/// The text of a path as plugins see it.
fn path_text(path: &ide_core::IpcPath) -> String {
    path.0.to_string_lossy().into_owned()
}

/// What signals that plugin `id` changed for its log tabs: its folder, version, enablement, whether
/// it can run, and its grants.
fn plugin_state(plugin: &ide_host::plugins::PluginInfo) -> String {
    let stopped = matches!(plugin.status, PluginStatus::Failed | PluginStatus::Disabled | PluginStatus::NeedsConsent);
    let grants: Vec<String> =
        plugin.capabilities.iter().map(|capability| format!("{}={}", capability.id, capability.granted)).collect();
    format!("{}|{}|{}|{stopped}|{}", plugin.path.0.display(), plugin.version, plugin.enabled, grants.join(","))
}

/// The tab icon of plugin log tabs.
pub(super) fn log_tab_icon(_: &EditorInput) -> String {
    PLUGIN_LOG_ICON.to_owned()
}

/// Builds the view of a log tab from its input; `None` for an input that is not a log tab's or
/// names no program. The window takes the tab's program in hand once the view is installed.
pub(super) fn build_plugin_log_tab(
    input: &EditorInput,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Option<TabView> {
    let data = LogTabData::of(input)?;
    if data.origin == LogOrigin::Code && data.program.trim().is_empty() {
        return None;
    }
    let key = input.key.clone();
    let view = cx.new(|cx| PluginLogTab::new(data, true, cx));
    let events = cx.subscribe_in(&view, window, {
        let key = key.clone();
        move |workbench, view, event: &PluginLogEvent, window, cx| match event {
            PluginLogEvent::Start { on } => workbench.start_plugin_log(&key, view.downgrade(), on.clone(), window, cx),
            PluginLogEvent::Stop => workbench.stop_plugin_log(&key, cx),
        }
    });
    let released = cx.observe_release(&view, {
        let key = key.clone();
        move |workbench, _, cx| workbench.plugin_log_released(&key, cx)
    });
    let installed = view.clone();
    // The workbench is being updated while the tab's view is built: the program is taken in hand
    // once that is over.
    cx.defer_in(window, move |workbench, _, cx| workbench.adopt_plugin_log(key, installed, released, cx));
    Some(
        TabView::new(crate::plugin_views::log_tab::PLUGIN_LOG_KIND, input.clone(), &view, cx).with_subscription(events),
    )
}

/// The log tab an action addresses: the tab whose key is its argument, else the active one.
fn log_target(arg: Option<&Value>, window: &Window, cx: &App) -> Option<Entity<PluginLogTab>> {
    let workbench = for_window(window, cx)?;
    let workbench = workbench.read(cx);
    match arg.and_then(Value::as_str) {
        Some(key) => workbench.plugin_runtime.logs.get(key).and_then(|session| session.view.upgrade()),
        None => workbench.active_tab_view().and_then(TabView::downcast::<PluginLogTab>),
    }
}

/// The log tab actions: Clear and Soft-wrap of a tab (its key as the argument, else the active tab)
/// and Toggle Log Option (`{ target?, id, checked }`).
pub(super) fn register_actions(registry: &mut ActionRegistry) {
    for (id, title) in LOG_OPERATIONS {
        let meta = ActionMeta { category: Some("Plugins".to_owned()), hidden: true, ..ActionMeta::new(id, title) };
        registry.register(Action::new(meta, move |call, window, cx| {
            let Some(tab) = log_target(call.arg, window, cx) else { return Ok(()) };
            tab.update(cx, |tab, cx| if id == "plugins.log.clear" { tab.clear(cx) } else { tab.toggle_wrap(cx) });
            Ok(())
        }));
    }
    registry.handle(TOGGLE_ARGUMENT, |call, window, cx| {
        let Some(arg) = call.arg else { return Ok(()) };
        let (Some(id), Some(checked)) =
            (arg.get("id").and_then(Value::as_str), arg.get("checked").and_then(Value::as_bool))
        else {
            return Ok(());
        };
        let Some(tab) = log_target(arg.get("target"), window, cx) else { return Ok(()) };
        let id = id.to_owned();
        tab.update(cx, |tab, cx| tab.set_toggle(&id, checked, cx));
        Ok(())
    });
}

impl Workbench {
    // ---------------------------------------------------------------------------------------------
    // Lifecycle

    /// Starts following the app's plugin host for this window. Call once, when the window is made;
    /// a window of an app without a plugin host follows nothing.
    pub(super) fn start_plugin_runtime(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugin_runtime.listeners.is_some() || !cx.has_global::<Plugins>() {
            return;
        }
        let handle = window.window_handle();
        let weak = cx.weak_entity();
        let plugins = cx.global_mut::<Plugins>().listen({
            let weak = weak.clone();
            move |client, event, cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    weak.update(cx, |workbench, cx| workbench.plugin_runtime_event(client, event, window, cx))
                });
            }
        });
        let backend = cx.has_global::<Backend>().then(|| {
            let weak = weak.clone();
            cx.global_mut::<Backend>().listen(move |event, cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    weak.update(cx, |workbench, cx| workbench.plugin_backend_event(event, window, cx))
                });
            })
        });
        let actions = cx.default_global::<ActionRegistry>().on_action_executed({
            let weak = weak.clone();
            move |id, _, source, cx| {
                let id = id.to_owned();
                let weak = weak.clone();
                // Actions run while their window may be being updated: the report waits until after.
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |workbench, cx| workbench.plugin_action_performed(&id, source, cx));
                });
            }
        });
        let observers = vec![
            cx.observe_global_in::<Plugins>(window, |workbench, window, cx| workbench.plugins_listed(window, cx)),
            cx.observe_in(&cx.entity(), window, |workbench, _, window, cx| workbench.check_plugin_project(window, cx)),
        ];
        self.plugin_runtime.listeners = Some(Listeners { plugins, backend, actions, _observers: observers });
        if let Some(host) = plugin_host(cx) {
            // Window labels are reused: this one may have been forgotten when an earlier window closed.
            host.reopen_client(&self.label);
        }
        self.plugins_listed(window, cx);
        self.check_plugin_project(window, cx);
    }

    /// The window goes: its listeners go, and the host forgets it (its prompts are cancelled, its
    /// context keys and queued events dropped, its log tab programs killed).
    pub(super) fn stop_plugin_runtime(&mut self, cx: &mut App) {
        let Some(listeners) = self.plugin_runtime.listeners.take() else { return };
        if cx.has_global::<Plugins>() {
            cx.global_mut::<Plugins>().unlisten(listeners.plugins);
        }
        if let (Some(id), true) = (listeners.backend, cx.has_global::<Backend>()) {
            cx.global_mut::<Backend>().unlisten(id);
        }
        if cx.has_global::<ActionRegistry>() {
            cx.global_mut::<ActionRegistry>().remove_listener(listeners.actions);
        }
        if let Some(host) = plugin_host(cx) {
            host.forget_client(&self.label);
        }
    }

    /// Whether the desktop command `command` may run for this window now (the project open and
    /// trusted, where the command needs it); the reason when it may not.
    pub(super) fn plugin_call_allowed(&self, command: &str, cx: &App) -> Result<(), String> {
        let backend = cx.try_global::<Backend>().ok_or_else(|| "the backend is not running".to_owned())?;
        backend.host.authorize_command(&self.label, command).map_err(|error| error.message)
    }

    /// The focused editor as a plugin call sees it: its file, language and selection.
    pub(super) fn plugin_editor(&self, cx: &App) -> Option<PluginEditor> {
        let editor = self.editor()?.read(cx);
        Some(PluginEditor {
            path: editor.path().to_string_lossy().into_owned(),
            language: editor.language().0.clone(),
            selections: vec![editor.selection_range()],
        })
    }

    /// The project root as the plugin runtime tracks it.
    fn plugin_project_root(&self, cx: &App) -> Option<String> {
        self.project_root(cx).map(|root| root.to_string_lossy().into_owned())
    }

    /// The window shows another project (or none): the status items and context keys are asked
    /// for again, as are the event kinds (the host answers them only for a trusted project); the
    /// `workspaceContains` keys follow the new root; the focused file is reported again; the log
    /// tabs of the previous project stop.
    fn check_plugin_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.plugin_project_root(cx);
        if self.plugin_runtime.root.as_ref() == Some(&root) {
            return;
        }
        let first = self.plugin_runtime.root.is_none();
        self.plugin_runtime.root = Some(root.clone());
        self.plugin_project_changed(!first, window, cx);
    }

    fn plugin_project_changed(&mut self, stop_logs: bool, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.plugin_runtime.root.clone().flatten();
        // A trusted project's own plugins load with it, and leave the listing when it goes.
        if cx.has_global::<Plugins>() {
            Plugins::refresh(cx);
        }
        self.plugin_runtime.status_revision += 1;
        let effects = self.plugin_runtime.ui.project_changed();
        self.apply_runtime_effects(effects, None, window, cx);
        self.refresh_plugin_status_items(cx);
        self.refresh_plugin_event_kinds(cx);
        let roots: Vec<String> = root.iter().cloned().collect();
        if let Some(request) = self.plugin_runtime.contains.set_roots(roots) {
            self.answer_workspace_contains(request, window, cx);
        }
        let current = self.plugin_editor(cx).map(|editor| editor.path);
        let report = match self.plugin_runtime.active_file.project_changed(root.as_deref(), current.as_deref()) {
            Some(report) => Some(report),
            None => self.plugin_runtime.active_file.focused(current.as_deref()),
        };
        if let Some(report) = report {
            self.report_active_file(report, cx);
        }
        if stop_logs {
            for session in self.plugin_runtime.logs.values() {
                if let Some(view) = session.view.upgrade() {
                    view.update(cx, |view, cx| view.stop(true, cx));
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Plugin events

    /// What the plugin host announces for this window (`client`; none: every window).
    fn plugin_runtime_event(
        &mut self,
        client: Option<&str>,
        event: &PluginEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if client.is_some_and(|client| client != self.label) {
            return;
        }
        let contributions = plugin_contributions(self.plugin_listing(cx));
        let handled = {
            let registry = cx.try_global::<ActionRegistry>();
            let has_action = |id: &str| registry.is_some_and(|registry| registry.contains(id));
            self.plugin_runtime.ui.handle(event, &contributions, &has_action)
        };
        if let Some(effects) = handled {
            let origin = match event {
                PluginEvent::Notification { plugin_id, .. } => Some(plugin_id.as_str()),
                _ => None,
            };
            self.apply_runtime_effects(effects, origin, window, cx);
            match event {
                PluginEvent::StatusItemChanged { .. } | PluginEvent::WindowStatusItemChanged { .. } => {
                    self.plugin_runtime.status_revision += 1;
                }
                PluginEvent::TaskStarted { task_id, .. }
                | PluginEvent::TaskProgress { task_id, .. }
                | PluginEvent::TaskEnded { task_id, .. } => self.sync_plugin_task(task_id, cx),
                _ => {}
            }
            return;
        }
        match event {
            PluginEvent::CommandsChanged | PluginEvent::SubscriptionsChanged => self.refresh_plugin_event_kinds(cx),
            PluginEvent::EditorOpen { path, selection } => {
                let start = selection.as_ref().map(|range| range.start).unwrap_or_default();
                self.open_plugin_location(path.0.clone(), start.line, start.col, window, cx);
            }
            PluginEvent::OpenLocation { path, line, column, .. } => {
                self.open_plugin_location(path.0.clone(), *line, *column, window, cx);
            }
            PluginEvent::EditorRanges { path, ranges, reveal } => self.set_plugin_ranges(path, ranges, *reveal, cx),
            PluginEvent::OpenProject { path, new_window, .. } => {
                let id = if *new_window { "file.openInNewWindow" } else { "file.openProjectPath" };
                let arg = Value::String(path.clone());
                window.defer(cx, move |window, cx| {
                    if let Err(error) = execute_action_or_throw(id, Some(&arg), window, cx) {
                        tracing::warn!("a plugin's project did not open: {error}");
                    }
                });
            }
            PluginEvent::PromptRequested { prompt_id, prompt, .. } => {
                self.show_plugin_prompt(prompt_id, prompt, window, cx);
            }
            PluginEvent::PromptClosed { prompt_id } => self.close_plugin_prompt(prompt_id, window, cx),
            PluginEvent::OpenLogTab { plugin_id, key, title, log } => {
                self.open_view_tab(code_log_input(plugin_id, key, title, log), window, cx);
            }
            PluginEvent::OutputChanged { plugin_id, channel, text } => {
                self.plugin_output_changed(plugin_id, channel, text.as_deref(), cx);
            }
            PluginEvent::RevealOutput { plugin_id, channel, text } => {
                self.plugin_output_changed(plugin_id, channel, Some(text), cx);
                let title =
                    cx.default_global::<Localization>().0.tx_built(&["Output: ", ""], &[Param::Text(channel.clone())]);
                self.open_view_tab(output_input(plugin_id, channel, &title), window, cx);
            }
            PluginEvent::WindowRequest {
                request_id, request: WindowRequest::Run { operation, config, debug }, ..
            } => {
                self.plugin_run_request(request_id, operation, config, *debug, window, cx);
            }
            PluginEvent::WindowRequest { request_id, request, .. } => {
                self.refuse_window_request(request_id, request, cx);
            }
            PluginEvent::CommitRequested { request_id, .. } => {
                // The Commit view does not hand its selection to plugins: a plugin hears what it
                // hears in a window without one.
                if self.plugin_call_allowed("plugin_answer_commit", cx).is_ok()
                    && let Some(host) = plugin_host(cx)
                {
                    host.answer_commit(request_id, CommitAnswer::NoRepository);
                }
            }
            PluginEvent::UpdatesAvailable { updates } => self.announce_plugin_updates(updates, cx),
            PluginEvent::OpenTerminal { plugin_id, title, .. } => {
                tracing::warn!("plugin {plugin_id} asked for the terminal {title:?}, which this window cannot run");
            }
            PluginEvent::ViewChanged { plugin_id, view } => self.plugin_view_changed(plugin_id, view, cx),
            PluginEvent::RevealView { plugin_id, view } => self.reveal_plugin_view(plugin_id, view, window, cx),
            PluginEvent::ViewMessage { plugin_id, view, .. } => self.plugin_view_message(plugin_id, view),
            // Only the kind is logged: such events carry what plugin views and pages exchange.
            other => tracing::debug!("plugin event not shown in this window: {}", plugin_event_kind(other)),
        }
    }

    /// Carries out what the runtime model asks; `origin` is the plugin a notification is from.
    fn apply_runtime_effects(
        &mut self,
        effects: Vec<RuntimeEffect>,
        origin: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for effect in effects {
            match effect {
                RuntimeEffect::StatusItemsChanged => self.sync_plugin_status_items(cx),
                // The task of the event is brought up to date by its id.
                RuntimeEffect::TasksChanged => {}
                RuntimeEffect::FetchContextKeys => self.fetch_plugin_context_keys(window, cx),
                RuntimeEffect::SetContext { name, value } => match value.as_ref().and_then(context_value) {
                    Some(value) => set_context(window, cx, &name, value),
                    None => clear_context(window, cx, &name),
                },
                RuntimeEffect::Notify(mut request) => {
                    if request.title.is_empty()
                        && let Some(plugin) = origin
                    {
                        request.title = plugin_name(plugin, self.plugin_listing(cx));
                    }
                    request.body = request.body.filter(|body| !body.is_empty());
                    self.notify(request, cx);
                }
                RuntimeEffect::CopyToClipboard(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                RuntimeEffect::OpenUrl(url) => cx.open_url(&url),
                RuntimeEffect::ExecuteAction { request_id, id, arg } => {
                    let label = self.label.clone();
                    window.defer(cx, move |window, cx| {
                        let arg = arg.map(Value::String);
                        let answer = match execute_action_or_throw(&id, arg.as_ref(), window, cx) {
                            Ok(()) => CommandAnswer::Value(None),
                            Err(error) => CommandAnswer::Failed(error.to_string()),
                        };
                        answer_plugin_command(&label, &request_id, answer, cx);
                    });
                }
                RuntimeEffect::RunPluginCommand { request_id, plugin_id, command, arg, chain } => {
                    let running = self.run_plugin_command(&plugin_id, &command, arg, chain, cx);
                    let label = self.label.clone();
                    cx.spawn(async move |_, cx| {
                        let answer = match running.await {
                            Ok(value) => CommandAnswer::Value(value),
                            Err(error) => CommandAnswer::Failed(error.message),
                        };
                        cx.update(|cx| answer_plugin_command(&label, &request_id, answer, cx));
                    })
                    .detach();
                }
                RuntimeEffect::NotFound { request_id } => {
                    answer_plugin_command(&self.label, &request_id, CommandAnswer::NotFound, cx);
                }
            }
        }
    }

    /// The plugin host's view of the window's context keys, read off the main thread.
    fn fetch_plugin_context_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let allowed = self.plugin_call_allowed("plugin_context_keys", cx);
        let host = plugin_host(cx);
        let label = self.label.clone();
        let work = cx.background_executor().spawn(async move {
            allowed?;
            let host = host.ok_or_else(|| "the plugin host is not running".to_owned())?;
            Ok::<_, String>(host.context_keys(Some(&label)))
        });
        cx.spawn_in(window, async move |workbench, cx| {
            let keys = work.await;
            let _ = workbench.update_in(cx, |workbench, window, cx| {
                let effects = workbench.plugin_runtime.ui.context_keys_fetched(keys);
                workbench.apply_runtime_effects(effects, None, window, cx);
            });
        })
        .detach();
    }

    // ---------------------------------------------------------------------------------------------
    // Status bar items

    /// Lists the status items of running plugin code again (later changes arrive as events). A
    /// listing that answers after another change is asked again.
    fn refresh_plugin_status_items(&mut self, cx: &mut Context<Self>) {
        self.plugin_runtime.status_request += 1;
        let (request, revision) = (self.plugin_runtime.status_request, self.plugin_runtime.status_revision);
        let Some(host) = plugin_host(cx) else { return };
        if let Err(reason) = self.plugin_call_allowed("plugin_status_items", cx) {
            tracing::debug!("plugin status items are not listed here: {reason}");
            return;
        }
        let label = self.label.clone();
        let work = cx.background_executor().spawn(async move { host.status_snapshot(&label) });
        cx.spawn(async move |workbench, cx| {
            let items = work.await;
            let _ = workbench.update(cx, |workbench, cx| workbench.plugin_status_listed(request, revision, items, cx));
        })
        .detach();
    }

    fn plugin_status_listed(
        &mut self,
        request: u64,
        revision: u64,
        items: Vec<ide_host::plugins::PluginStatusItem>,
        cx: &mut Context<Self>,
    ) {
        if request != self.plugin_runtime.status_request {
            return;
        }
        if revision != self.plugin_runtime.status_revision {
            self.refresh_plugin_status_items(cx);
            return;
        }
        let ui = &mut self.plugin_runtime.ui;
        let shown: Vec<(String, String)> = ui
            .status_items()
            .into_iter()
            .filter_map(|(key, shown)| {
                let id = key.strip_prefix(&format!("{}/", shown.plugin_id))?.to_owned();
                Some((shown.plugin_id, id))
            })
            .collect();
        let no_action = |_: &str| false;
        for (plugin_id, id) in shown {
            let global = PluginEvent::StatusItemChanged { plugin_id: plugin_id.clone(), id: id.clone(), item: None };
            let local = PluginEvent::WindowStatusItemChanged { plugin_id, id, item: None };
            ui.handle(&global, &[], &no_action);
            ui.handle(&local, &[], &no_action);
        }
        for listed in items {
            let ide_host::plugins::PluginStatusItem { window, plugin_id, id, item } = listed;
            let event = if window {
                PluginEvent::WindowStatusItemChanged { plugin_id, id, item: Some(item) }
            } else {
                PluginEvent::StatusItemChanged { plugin_id, id, item: Some(item) }
            };
            ui.handle(&event, &[], &no_action);
        }
        self.sync_plugin_status_items(cx);
    }

    /// Registers one status bar item per plugin item (declared, or set by code; the code's wins),
    /// and takes away the ones that went.
    fn sync_plugin_status_items(&mut self, cx: &mut Context<Self>) {
        let running = self.plugin_runtime.ui.status_items();
        let declared = self.plugin_runtime.declared.clone();
        let keys: BTreeSet<&String> = running.keys().chain(declared.keys()).collect();
        // The items of the plugins this window sees: another project's own plugins show none here.
        let listed = self.plugin_listing(cx).cloned();
        let listed = listed.as_ref();
        let seen = |plugin_id: &str| listed.is_some_and(|listed| listed.plugins.iter().any(|p| p.id == plugin_id));
        let mut wanted: BTreeMap<String, (StatusItemSpec, StatusText)> = BTreeMap::new();
        for key in keys {
            let declaration = declared.get(key);
            let Some((plugin_id, item)) = running
                .get(key)
                .map(|shown| (shown.plugin_id.clone(), shown.item.clone()))
                .or_else(|| declaration.map(|d| (d.plugin_id.clone(), d.item.clone())))
            else {
                continue;
            };
            if !seen(&plugin_id) {
                continue;
            }
            let id = format!("{PLUGIN_STATUS_PREFIX}{key}");
            let name = plugin_name(&plugin_id, listed);
            let what = declaration
                .map(|d| d.item.tooltip.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| d.item.text.clone()));
            let title = match what.filter(|what| !what.is_empty()) {
                Some(what) => format!("{name}: {what}"),
                None => name,
            };
            let (align, priority) =
                declaration.map_or((StatusAlign::Right, DEFAULT_PRIORITY), |d| (d.align, d.priority));
            let spec = StatusItemSpec::new(&id, align, priority).title(&title);
            let mut text = StatusText::new(item.text.clone());
            if let Some(tooltip) = item.tooltip.clone() {
                text = text.tooltip(tooltip);
            }
            if let Some(command) = &item.command {
                text = text.on_click(StatusClick::action(&plugin_action_id(&plugin_id, command)));
            }
            wanted.insert(id, (spec, text));
        }
        let bar = self.status_bar().clone();
        let gone: Vec<String> =
            self.plugin_runtime.shown_items.keys().filter(|id| !wanted.contains_key(*id)).cloned().collect();
        bar.update(cx, |bar, cx| {
            for id in &gone {
                bar.unregister(id, cx);
            }
        });
        for id in gone {
            self.plugin_runtime.shown_items.remove(&id);
        }
        for (id, (spec, text)) in wanted {
            let known = self.plugin_runtime.shown_items.get(&id) == Some(&spec);
            bar.update(cx, |bar, cx| {
                if known {
                    bar.set_text(&id, Some(text), cx);
                } else {
                    bar.register(spec.clone(), StatusContent::Text(text), cx);
                }
            });
            self.plugin_runtime.shown_items.insert(id, spec);
        }
    }

    /// A click on plugin status item `item` (`plugin.<plugin>/<id>`): its plugin's command runs.
    pub(super) fn plugin_status_item_clicked(&mut self, item: &str, cx: &mut Context<Self>) {
        let Some(key) = item.strip_prefix(PLUGIN_STATUS_PREFIX) else { return };
        let running = self.plugin_runtime.ui.status_items();
        let target = running
            .get(key)
            .map(|shown| (shown.plugin_id.clone(), shown.item.command.clone()))
            .or_else(|| self.plugin_runtime.declared.get(key).map(|d| (d.plugin_id.clone(), d.item.command.clone())));
        let Some((plugin_id, Some(command))) = target else { return };
        self.run_plugin_command_reported(&plugin_id, &command, None, cx);
    }

    // ---------------------------------------------------------------------------------------------
    // Background tasks

    /// Brings the plugin task `task_id` up to date in the window's task list: listed while it
    /// runs, finished when it ends.
    fn sync_plugin_task(&mut self, task_id: &str, cx: &mut Context<Self>) {
        let id = format!("plugin:{task_id}");
        let task = self.plugin_runtime.ui.tasks().iter().find(|task| task.id == id).cloned();
        match task {
            Some(task) => {
                let cancel = if task.cancellable { Change::Set(Cancel::Producer) } else { Change::Clear };
                let update = TaskUpdate {
                    message: task.message.clone(),
                    fraction: task.fraction.map(|fraction| fraction.min(1.0)),
                    done: task.done,
                    cancel,
                    uncancellable: Change::Clear,
                    ..TaskUpdate::new(TaskId::Name(id), &task.title)
                };
                self.tasks.upsert_task(update, now_ms());
            }
            None => self.tasks.remove_task(&TaskId::Name(id)),
        }
        self.schedule_tasks(cx);
        cx.notify();
    }

    /// Cancel of a background task: it shows "Cancelling…" until its producer ends it. A plugin's
    /// task is cancelled through the plugin host, the backend's own through its cancel handle.
    pub fn cancel_background_task(&mut self, id: &TaskId, cx: &mut Context<Self>) {
        let effect = self.tasks.cancel_task(id);
        match effect {
            Some(CancelEffect::Backend(task)) => match u32::try_from(task) {
                Ok(task) => {
                    if !ide_core::task::cancel(ide_core::TaskId(task)) {
                        tracing::debug!("background task {task} had ended or cannot be cancelled");
                    }
                }
                Err(_) => tracing::warn!("background task {task} is not one the backend numbers"),
            },
            Some(CancelEffect::Producer(TaskId::Name(name))) if name.starts_with("plugin:") => {
                let task = name.trim_start_matches("plugin:").to_owned();
                match (self.plugin_call_allowed("plugin_cancel_task", cx), plugin_host(cx)) {
                    (Ok(()), Some(host)) => {
                        if !host.cancel_task(&task) {
                            tracing::debug!("plugin task {task} had ended");
                        }
                    }
                    (Err(reason), _) => tracing::warn!("plugin task {task} is not cancelled here: {reason}"),
                    (_, None) => tracing::warn!("plugin task {task}: the plugin host is not running"),
                }
            }
            Some(CancelEffect::Producer(other)) => tracing::warn!("task {other:?} has no producer to stop it"),
            Some(CancelEffect::Dequeued(_)) | None => {}
        }
        self.schedule_tasks(cx);
        cx.notify();
    }

    // ---------------------------------------------------------------------------------------------
    // Commands

    /// Runs command `command` of plugin `plugin_id` for this window, off the main thread: its
    /// project, its focused editor, the argument and the chain of commands that asked.
    fn run_plugin_command(
        &self,
        plugin_id: &str,
        command: &str,
        arg: Option<String>,
        chain: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<IdeResult<Option<String>>> {
        // The refusal keeps its kind: a project in Restricted Mode is told as the desktop tells it.
        let allowed = match cx.try_global::<Backend>() {
            Some(backend) => backend.host.authorize_command(&self.label, "plugin_run_command"),
            None => Err(ide_core::IdeError::internal("the backend is not running")),
        };
        let host = plugin_host(cx);
        let invocation = Invocation {
            command_chain: chain,
            client: Some(self.label.clone()),
            workspace_root: self.project_root(cx),
            editor: self.plugin_editor(cx).map(PluginEditor::context),
            selected_paths: Vec::new(),
            global_settings: false,
        };
        let (plugin, command) = (plugin_id.to_owned(), command.to_owned());
        crate::plugins::host_blocking(cx, move || {
            allowed?;
            let host = host.ok_or_else(|| ide_core::IdeError::internal("the plugin host is not running"))?;
            host.run_command(&plugin, &command, arg.as_deref(), invocation)
        })
    }

    /// Runs a plugin command the user asked for (a status item, a notification button); a failure
    /// is told in the window unless the host already said why (the plugin stopped, or it asks for
    /// a permission).
    fn run_plugin_command_reported(
        &mut self,
        plugin_id: &str,
        command: &str,
        arg: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let running = self.run_plugin_command(plugin_id, command, arg, Vec::new(), cx);
        let title = command_title(&plugin_contributions(self.plugin_listing(cx)), plugin_id, command);
        let (plugin, host) = (plugin_id.to_owned(), plugin_host(cx));
        cx.spawn(async move |workbench, cx| {
            let Err(error) = running.await else { return };
            let _ = workbench.update(cx, |workbench, cx| {
                Plugins::refresh(cx);
                let status = host.and_then(|host| host.info_of(&plugin).ok()).map(|p| p.status);
                let i18n = &mut cx.default_global::<Localization>().0;
                if let Some(request) = command_failure(&title, &error, status, i18n) {
                    workbench.notify(request, cx);
                }
            });
        })
        .detach();
    }

    /// A button of a plugin's notification was pressed: the command it names runs in the
    /// publishing plugin, with its argument.
    pub(super) fn run_plugin_notification_button(&mut self, command: &str, cx: &mut Context<Self>) {
        let Some(call) = PluginCommandCall::parse(command) else {
            tracing::warn!("a plugin notification button carries no command");
            return;
        };
        self.run_plugin_command_reported(&call.plugin_id, &call.command, call.argument, cx);
    }

    /// The plugin host says newer versions of installed plugins are available.
    fn announce_plugin_updates(&mut self, updates: &[PluginUpdate], cx: &mut Context<Self>) {
        let names: Vec<String> = updates.iter().map(|update| format!("{} {}", update.name, update.version)).collect();
        let title = if updates.len() == 1 {
            "Plugin update available".to_owned()
        } else {
            format!("{} plugin updates available", updates.len())
        };
        let actions = vec![
            NotificationAction { label: "Update All".to_owned(), command: "plugins.updateAll".to_owned() },
            NotificationAction { label: "Show Plugins".to_owned(), command: "plugins.settings".to_owned() },
        ];
        let request = NotificationRequest {
            body: Some(names.join(", ")),
            group: Some("plugins".to_owned()),
            actions,
            ..NotificationRequest::new(NotificationLevel::Info, title)
        };
        self.notify(request, cx);
    }

    /// A window-owned operation this window does not carry out (runs, terminals, database
    /// connections, content tabs): the plugin hears why, as it would from a window without those
    /// services, and an asynchronous run is reported as never started.
    fn refuse_window_request(&mut self, request_id: &str, request: &WindowRequest, cx: &mut Context<Self>) {
        let reason = match request {
            WindowRequest::Run { operation, .. } => {
                if self.plugin_call_allowed("plugin_finish_operation", cx).is_ok()
                    && let Some(host) = plugin_host(cx)
                {
                    host.finish_operation(&self.label, operation, None, false);
                }
                "Run service unavailable".to_owned()
            }
            WindowRequest::Terminal { .. } => "Terminal service unavailable".to_owned(),
            WindowRequest::DatabaseConnect { .. } => {
                cx.default_global::<Localization>().0.tx("Database service unavailable")
            }
            WindowRequest::ContentTab { .. } => "Editor unavailable".to_owned(),
        };
        tracing::warn!("a plugin's window request was refused: {reason}");
        if !request_id.is_empty() {
            answer_plugin_command(&self.label, request_id, CommandAnswer::Failed(reason), cx);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Files

    /// Opens `path` with the caret at `line`:`column` (0-based, UTF-16), in a tab of the project or
    /// on its own.
    fn open_plugin_location(
        &mut self,
        path: PathBuf,
        line: u32,
        column: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project_tree().is_some() {
            self.open_project_file_at(path, line, column, window, cx);
        } else {
            self.open_file(path, window, cx);
        }
    }

    /// Selects `ranges` (or, with `reveal`, scrolls to the first) in the open editors of `path`. A
    /// range outside a document leaves that editor as it is.
    fn set_plugin_ranges(
        &mut self,
        path: &ide_core::IpcPath,
        ranges: &[ide_core::Range],
        reveal: bool,
        cx: &mut Context<Self>,
    ) {
        use crate::editor::buffer::{line_count, line_range};
        use crate::editor_lsp::positions::pos_to_offset;
        if ranges.is_empty() {
            return;
        }
        let editors = self.editors().into_iter().filter(|editor| editor.read(cx).path() == path.0.as_path());
        for editor in editors.collect::<Vec<_>>() {
            editor.update(cx, |editor, cx| {
                let offsets: Option<Vec<(u32, u32)>> = {
                    let rope = editor.rope();
                    let lines = line_count(rope);
                    let fits = |pos: ide_core::Pos| {
                        pos.line < lines && {
                            let span = line_range(rope, pos.line);
                            pos.col <= span.end - span.start
                        }
                    };
                    ranges.iter().all(|range| fits(range.start) && fits(range.end)).then(|| {
                        ranges
                            .iter()
                            .map(|range| (pos_to_offset(rope, range.start), pos_to_offset(rope, range.end)))
                            .collect()
                    })
                };
                let Some(offsets) = offsets else { return };
                if reveal {
                    editor.scroll_to_line(ranges[0].start.line, cx);
                } else {
                    editor.set_selections(offsets, 0, cx);
                }
            });
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Questions

    /// A plugin's command waits for the user: its question shows over the window (a folder is
    /// chosen in the system's dialog).
    fn show_plugin_prompt(&mut self, prompt_id: &str, prompt: &Prompt, window: &mut Window, cx: &mut Context<Self>) {
        let view = match prompt {
            Prompt::PickDirectory { .. } => {
                self.pick_plugin_folder(prompt_id, window, cx);
                return;
            }
            Prompt::QuickPick { title, items, multiple } => {
                let (title, items, multiple) = (title.clone(), items.clone(), *multiple);
                cx.new(move |cx| PluginPrompt::pick(&title, items, multiple, cx))
            }
            Prompt::Input { prompt, value, multiline } => {
                cx.new(|cx| PluginPrompt::input(prompt, value.as_deref(), *multiline, window, cx))
            }
            Prompt::Confirm { title, message } => cx.new(|cx| PluginPrompt::confirm_question(title, message, cx)),
        };
        let id = prompt_id.to_owned();
        let events = cx.subscribe_in(&view, window, move |workbench, _, event: &PluginPromptEvent, window, cx| {
            let PluginPromptEvent::Answered(answer) = event;
            workbench.answer_plugin_prompt(&id, answer.clone(), window, cx);
        });
        let restore = match self.plugin_runtime.prompts.first() {
            Some(first) => first.restore.clone(),
            None => window.focused(cx),
        };
        window.focus(&view.focus_handle(cx), cx);
        let prompt_id = prompt_id.to_owned();
        self.plugin_runtime.prompts.push(OpenPrompt { prompt_id, view, restore, _events: events });
        cx.notify();
    }

    /// The user answered (`None`: dismissed): the question goes and the plugin gets the answer.
    fn answer_plugin_prompt(
        &mut self,
        prompt_id: &str,
        answer: Option<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.remove_plugin_prompt(prompt_id, window, cx) {
            return;
        }
        self.send_prompt_answer(prompt_id, answer, cx);
    }

    fn send_prompt_answer(&self, prompt_id: &str, answer: Option<Vec<String>>, cx: &App) {
        if let Err(reason) = self.plugin_call_allowed("plugin_answer_prompt", cx) {
            tracing::warn!("a plugin's question is not answered here: {reason}");
            return;
        }
        let Some(host) = plugin_host(cx) else { return };
        if !host.answer_prompt(prompt_id, answer) {
            tracing::debug!("plugin question {prompt_id} no longer waits");
        }
    }

    /// The command that asked gave up (cancelled, out of time, its plugin stopped, its window
    /// gone): the question closes unanswered.
    fn close_plugin_prompt(&mut self, prompt_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.plugin_runtime.folder_prompts.remove(prompt_id);
        self.remove_plugin_prompt(prompt_id, window, cx);
    }

    /// Takes question `prompt_id` off the screen; the keyboard goes back where it was once no
    /// question is left. Whether it was there.
    fn remove_plugin_prompt(&mut self, prompt_id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.plugin_runtime.prompts.iter().position(|open| open.prompt_id == prompt_id) else {
            return false;
        };
        let removed = self.plugin_runtime.prompts.remove(index);
        let next = self.plugin_runtime.prompts.last().map(|open| open.view.focus_handle(cx));
        match next.or(removed.restore) {
            Some(target) => window.focus(&target, cx),
            None => {
                if let Some(editor) = self.editor() {
                    window.focus(&editor.focus_handle(cx), cx);
                }
            }
        }
        cx.notify();
        true
    }

    /// A folder question: the system's dialog, its choice as the answer (none: dismissed).
    fn pick_plugin_folder(&mut self, prompt_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.plugin_runtime.folder_prompts.insert(prompt_id.to_owned());
        let picked =
            cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
        let id = prompt_id.to_owned();
        cx.spawn_in(window, async move |workbench, cx| {
            let chosen = picked.await;
            let _ = workbench.update(cx, |workbench, cx| {
                // The plugin gave up meanwhile: nothing waits for the choice.
                if !workbench.plugin_runtime.folder_prompts.remove(&id) {
                    return;
                }
                let answer = match chosen {
                    Ok(Ok(Some(paths))) => paths.first().map(|path| vec![path.to_string_lossy().into_owned()]),
                    Ok(Ok(None)) | Err(_) => None,
                    Ok(Err(error)) => {
                        let title = cx.default_global::<Localization>().0.tx("Cannot choose a directory");
                        let request = NotificationRequest {
                            body: Some(error.to_string()),
                            ..NotificationRequest::new(NotificationLevel::Error, title)
                        };
                        workbench.notify(request, cx);
                        None
                    }
                };
                workbench.send_prompt_answer(&id, answer, cx);
            });
        })
        .detach();
    }

    /// Whether the window told the plugin host the settings since it knew a plugin wants them: the
    /// host keeps what it was told as each plugin's baseline, and a later change goes out as the keys
    /// that differ from it.
    #[cfg(test)]
    pub(super) fn plugin_settings_told(&self) -> bool {
        let runtime = &self.plugin_runtime;
        runtime.wanted.contains(&EventKind::SettingsChanged) && runtime.queue.is_empty()
    }

    /// The question on screen, the newest when several wait.
    pub fn plugin_prompt(&self) -> Option<&Entity<PluginPrompt>> {
        self.plugin_runtime.prompts.last().map(|open| &open.view)
    }

    /// The question over the window, while one waits.
    pub(super) fn plugin_prompt_layer(&self) -> Option<AnyElement> {
        let view = self.plugin_runtime.prompts.last()?.view.clone();
        let layer = div()
            .id("plugin-prompt-layer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .child(view);
        Some(gpui_kit::deferred(layer).with_priority(1).into_any_element())
    }

    // ---------------------------------------------------------------------------------------------
    // Log tabs and output channels

    /// A log tab's view was installed: the window owns its program from now on, and starts it (an
    /// output tab shows the channel's text instead).
    fn adopt_plugin_log(
        &mut self,
        key: String,
        view: Entity<PluginLogTab>,
        released: Subscription,
        cx: &mut Context<Self>,
    ) {
        let session =
            LogSession { view: view.downgrade(), terminal: None, generation: 0, forward: None, _released: released };
        self.plugin_runtime.logs.insert(key, session);
        let data = view.read(cx).data().clone();
        match data.origin {
            LogOrigin::Code => view.update(cx, |view, cx| view.start(cx)),
            LogOrigin::Output => {
                let channel = (data.plugin_id.clone(), data.key.clone().unwrap_or_default());
                let text = self.plugin_runtime.outputs.get(&channel).cloned().unwrap_or_default();
                view.update(cx, |view, cx| view.set_output(&text, cx));
            }
        }
    }

    /// Starts the program of log tab `key` with the toggles `on`, through the plugin's grant; its
    /// output goes to the tab.
    fn start_plugin_log(
        &mut self,
        key: &str,
        view: WeakEntity<PluginLogTab>,
        on: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_plugin_log(key, cx);
        let Some(tab) = view.upgrade() else { return };
        let data = tab.read(cx).data().clone();
        let allowed = self.plugin_call_allowed("plugin_log_tab_spawn", cx);
        let (plugins, backend) = (plugin_host(cx), cx.try_global::<Backend>().map(|backend| backend.host.clone()));
        let (Ok(()), Some(plugins), Some(backend)) = (allowed.clone(), plugins, backend) else {
            let reason = allowed.err().unwrap_or_else(|| "the plugin host is not running".to_owned());
            tab.update(cx, |tab, cx| tab.failed(&reason, cx));
            return;
        };
        let Some(session) = self.plugin_runtime.logs.get_mut(key) else { return };
        session.generation += 1;
        let generation = session.generation;
        let (sender, mut output) = futures::channel::mpsc::unbounded::<String>();
        let emit: ide_host::ops::Emit<String> = Arc::new(move |chunk| {
            // The tab is gone once the receiver is: its output has nowhere to go.
            let _ = sender.unbounded_send(chunk);
        });
        let label = self.label.clone();
        let log_key = data.key.clone().unwrap_or_default();
        let spawned = cx.background_executor().spawn(async move {
            plugins.spawn_log_tab_pty(&backend, &data.plugin_id, &log_key, &label, &on, (LOG_COLUMNS, LOG_ROWS), emit)
        });
        let tab_key = key.to_owned();
        session.forward = Some(cx.spawn_in(window, async move |workbench, cx| {
            match spawned.await {
                Ok(info) => {
                    // A start that was stopped or replaced meanwhile leaves no program behind.
                    let stale = workbench
                        .update(cx, |workbench, cx| {
                            let current =
                                workbench.plugin_runtime.logs.get_mut(&tab_key).filter(|s| s.generation == generation);
                            match current {
                                Some(session) => {
                                    session.terminal = Some(info.id);
                                    if let Some(code) = workbench.plugin_runtime.early_exits.remove(&info.id) {
                                        workbench.plugin_terminal_exited(info.id, code, cx);
                                    }
                                    None
                                }
                                None => Some(cx.try_global::<Backend>().map(|backend| backend.host.clone())),
                            }
                        })
                        .unwrap_or(Some(None));
                    if let Some(host) = stale {
                        kill_terminal(info.id, cx.background_executor(), host);
                        return;
                    }
                    while let Some(chunk) = output.next().await {
                        if view.update(cx, |tab, cx| tab.append(&chunk, cx)).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = view.update(cx, |tab, cx| tab.failed(&error.message, cx));
                }
            }
        }));
    }

    /// Kills the program of log tab `key`, if it runs.
    fn stop_plugin_log(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(session) = self.plugin_runtime.logs.get_mut(key) else { return };
        session.forward = None;
        session.generation += 1;
        let terminal = session.terminal.take();
        let host = cx.try_global::<Backend>().map(|backend| backend.host.clone());
        if let Some(terminal) = terminal {
            kill_terminal(terminal, cx.background_executor(), host);
        }
    }

    /// Log tab `key` closed: its program is killed.
    fn plugin_log_released(&mut self, key: &str, cx: &mut Context<Self>) {
        self.stop_plugin_log(key, cx);
        self.plugin_runtime.logs.remove(key);
    }

    /// A log tab's program ended.
    fn plugin_terminal_exited(&mut self, terminal: TerminalId, code: Option<i32>, cx: &mut Context<Self>) {
        let ended = self.plugin_runtime.logs.values_mut().find(|session| session.terminal == Some(terminal));
        let Some(session) = ended else {
            // Possibly a log tab's program whose start is not answered yet; kept for a few.
            if self.plugin_runtime.logs.values().any(|session| session.forward.is_some() && session.terminal.is_none())
            {
                if self.plugin_runtime.early_exits.len() >= MAX_EARLY_EXITS {
                    self.plugin_runtime.early_exits.clear();
                }
                self.plugin_runtime.early_exits.insert(terminal, code);
            }
            return;
        };
        session.terminal = None;
        if let Some(view) = session.view.upgrade() {
            view.update(cx, |view, cx| view.exited(code, cx));
        }
    }

    /// An output channel's snapshot changed (`None`: withdrawn); its open tabs show it.
    fn plugin_output_changed(&mut self, plugin_id: &str, channel: &str, text: Option<&str>, cx: &mut Context<Self>) {
        let id = (plugin_id.to_owned(), channel.to_owned());
        match text {
            Some(text) => self.plugin_runtime.outputs.insert(id, text.to_owned()),
            None => self.plugin_runtime.outputs.remove(&id),
        };
        let text = text.unwrap_or_default();
        for session in self.plugin_runtime.logs.values() {
            let Some(view) = session.view.upgrade() else { continue };
            let data = view.read(cx).data().clone();
            if data.origin == LogOrigin::Output && data.plugin_id == plugin_id && data.key.as_deref() == Some(channel) {
                view.update(cx, |view, cx| view.set_output(text, cx));
            }
        }
    }

    /// The log tabs open in the window, by tab key.
    pub fn plugin_log_tabs(&self) -> Vec<(String, Entity<PluginLogTab>)> {
        self.plugin_runtime
            .logs
            .iter()
            .filter_map(|(key, session)| Some((key.clone(), session.view.upgrade()?)))
            .collect()
    }

    // ---------------------------------------------------------------------------------------------
    // Listings and workbench events

    /// The plugins were listed again: the declared status items, the `workspaceContains` keys
    /// their `when` clauses name, and the log tabs of a plugin that changed.
    fn plugins_listed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(listed) = self.plugin_listing(cx) else { return };
        let contributions = Contribution::from_snapshot(listed);
        let states: HashMap<String, String> =
            listed.plugins.iter().map(|plugin| (plugin.id.clone(), plugin_state(plugin))).collect();
        let changed: Vec<String> = self
            .plugin_runtime
            .plugin_states
            .iter()
            .filter(|(id, state)| states.get(*id) != Some(*state))
            .map(|(id, _)| id.clone())
            .collect();
        self.plugin_runtime.plugin_states = states;
        for session in self.plugin_runtime.logs.values() {
            let Some(view) = session.view.upgrade() else { continue };
            if changed.contains(&view.read(cx).data().plugin_id) {
                view.update(cx, |view, cx| view.stop(true, cx));
            }
        }
        let declared = declared_items(&contributions);
        if declared != self.plugin_runtime.declared {
            self.plugin_runtime.declared = declared;
            self.sync_plugin_status_items(cx);
        }
        let keys = crate::plugin_models::workspace_contains::workspace_contains_keys(
            contributions.iter().map(Contribution::when),
        );
        if keys != self.plugin_runtime.contains.keys() {
            let request = self.plugin_runtime.contains.set_contributions(&contributions);
            self.answer_workspace_contains(request, window, cx);
        }
    }

    /// Lists the folders `request` names off the main thread and sets the keys from what they
    /// hold; an answer to an older request is dropped. A folder that cannot be listed holds
    /// nothing.
    fn answer_workspace_contains(&mut self, request: ContainsRequest, window: &mut Window, cx: &mut Context<Self>) {
        let allowed = self.plugin_call_allowed("fs_read_dir", cx).is_ok();
        let dirs = if allowed { request.dirs.clone() } else { Vec::new() };
        let Some(backend) = cx.try_global::<Backend>() else { return };
        let label = self.label.clone();
        let listing = backend.with_host(move |host| async move {
            let mut listings = HashMap::new();
            for dir in dirs {
                if let Ok(entries) = ide_host::ops::fs::read_dir(&host, &label, PathBuf::from(&dir)).await {
                    listings.insert(dir, entries.into_iter().map(|entry| entry.name).collect::<Vec<_>>());
                }
            }
            listings
        });
        cx.spawn_in(window, async move |workbench, cx| {
            let listings = listing.await.unwrap_or_default();
            let _ = workbench.update_in(cx, |workbench, window, cx| {
                let Some(values) = workbench.plugin_runtime.contains.answer(&request, &listings) else { return };
                for (key, value) in values {
                    set_context(window, cx, &key, value);
                }
            });
        })
        .detach();
    }

    /// What the backend tells the window that plugins hear of: repositories, settings, files and
    /// ignored folders changed, the project's trust, and the ends of log tab programs.
    fn plugin_backend_event(&mut self, event: &BackendEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.window.as_deref().is_some_and(|label| label != self.label) {
            return;
        }
        match &event.event {
            IdeEvent::VcsChanged { root } => self.post_plugin_event(WorkbenchEvent::VcsChanged(path_text(root)), cx),
            IdeEvent::SettingsChanged { .. } => self.post_plugin_event(WorkbenchEvent::SettingsChanged(Vec::new()), cx),
            IdeEvent::FsChanged { created, removed, .. } => {
                let created: Vec<String> = created.iter().map(path_text).collect();
                let removed: Vec<String> = removed.iter().map(path_text).collect();
                if let Some(request) = self.plugin_runtime.contains.files_changed(&created, &removed) {
                    self.answer_workspace_contains(request, window, cx);
                }
            }
            IdeEvent::ExcludedDirsChanged { dirs } => {
                let dirs: Vec<String> = dirs.iter().map(path_text).collect();
                if let Some(request) = self.plugin_runtime.contains.excluded_dirs_changed(&dirs) {
                    self.answer_workspace_contains(request, window, cx);
                }
            }
            IdeEvent::ProjectTrustChanged { root, .. } => {
                if self.plugin_project_root(cx).as_deref() == Some(path_text(root).as_str()) {
                    self.plugin_project_changed(false, window, cx);
                }
            }
            // The backend clears a document's dirty flag only when it saved it.
            IdeEvent::DocDirty { doc, dirty: false } => self.plugin_document_saved(*doc, cx),
            IdeEvent::TerminalExited { terminal, code } => self.plugin_terminal_exited(*terminal, *code, cx),
            _ => {}
        }
    }

    /// Asks the plugin host again which workbench events plugins want; with `settings-changed`
    /// among them, the plugins hear the settings once (they may have missed a change before).
    fn refresh_plugin_event_kinds(&mut self, cx: &mut Context<Self>) {
        let Some(host) = plugin_host(cx) else { return };
        if let Err(reason) = self.plugin_call_allowed("plugin_event_kinds", cx) {
            tracing::debug!("plugin event kinds are not asked here: {reason}");
            return;
        }
        let work = cx.background_executor().spawn(async move {
            host.wait_initial_load(INITIAL_LOAD_WAIT);
            host.event_kinds()
        });
        cx.spawn(async move |workbench, cx| {
            let kinds = work.await;
            let _ = workbench.update(cx, |workbench, cx| {
                workbench.plugin_runtime.wanted = kinds;
                if workbench.plugin_runtime.wanted.contains(&EventKind::SettingsChanged) {
                    workbench.post_plugin_event(WorkbenchEvent::SettingsChanged(Vec::new()), cx);
                }
            });
        })
        .detach();
    }

    /// Queues `event` for the plugins that want its kind; the queue goes out every
    /// [`POST_INTERVAL`], with the focused editor.
    pub(super) fn post_plugin_event(&mut self, event: WorkbenchEvent, cx: &mut Context<Self>) {
        if !self.plugin_runtime.wanted.contains(&event.kind()) {
            return;
        }
        self.plugin_runtime.queue.push(event);
        if self.plugin_runtime.post_timer.is_some() {
            return;
        }
        self.plugin_runtime.post_timer = Some(cx.spawn(async move |workbench, cx| {
            cx.background_executor().timer(POST_INTERVAL).await;
            let _ = workbench.update(cx, |workbench, cx| workbench.flush_plugin_events(cx));
        }));
    }

    fn flush_plugin_events(&mut self, cx: &mut Context<Self>) {
        self.plugin_runtime.post_timer = None;
        let batch = std::mem::take(&mut self.plugin_runtime.queue);
        if batch.is_empty() {
            return;
        }
        if let Err(reason) = self.plugin_call_allowed("plugin_post_events", cx) {
            tracing::debug!("workbench events are not posted here: {reason}");
            return;
        }
        let Some(host) = plugin_host(cx) else { return };
        let (label, editor) = (self.label.clone(), self.plugin_editor(cx));
        cx.background_executor().spawn(async move { host.post_window_events(&label, batch, editor) }).detach();
    }

    /// Reports the focused editor's file (`None`: no editor) as `editor-focused`.
    fn report_active_file(&self, path: Option<String>, cx: &mut Context<Self>) {
        if let Err(reason) = self.plugin_call_allowed("plugin_set_active_file", cx) {
            tracing::debug!("the focused file is not reported: {reason}");
            return;
        }
        let Some(host) = plugin_host(cx) else { return };
        let label = self.label.clone();
        cx.background_executor().spawn(async move { host.set_active_file(&label, path) }).detach();
    }

    /// The tabs changed: files that opened or closed are reported, and the focused file when it is
    /// another one.
    pub(super) fn plugin_tabs_changed(&mut self, cx: &mut Context<Self>) {
        // A window that follows no plugin host has nobody to tell.
        if self.plugin_runtime.listeners.is_none() {
            return;
        }
        let open: BTreeSet<String> = self
            .tabs()
            .map(|tabs| {
                tabs.panes
                    .iter()
                    .flat_map(|pane| pane.tabs.iter())
                    .filter(|tab| tab.input.kind == super::tab_views::FILE_KIND)
                    .filter_map(|tab| tab.input.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        let opened: Vec<String> = open.difference(&self.plugin_runtime.open_files).cloned().collect();
        let closed: Vec<String> = self.plugin_runtime.open_files.difference(&open).cloned().collect();
        self.plugin_runtime.open_files = open;
        for path in opened {
            self.post_plugin_event(WorkbenchEvent::FileOpened(path), cx);
        }
        for path in closed {
            self.post_plugin_event(WorkbenchEvent::FileClosed(path), cx);
        }
        let active = self
            .tabs()
            .and_then(|tabs| tabs.pane(&tabs.active_pane))
            .and_then(|pane| pane.active_tab())
            .filter(|tab| tab.input.kind == super::tab_views::FILE_KIND)
            .and_then(|tab| tab.input.path.clone());
        if let Some(report) = self.plugin_runtime.active_file.focused(active.as_deref()) {
            self.report_active_file(report, cx);
        }
        self.plugin_activation_events(cx);
        self.plugin_gutter_shown(cx);
    }

    /// The user ran action `id`: plugins that want `action-performed` hear of it.
    fn plugin_action_performed(&mut self, id: &str, source: ActionSource, cx: &mut Context<Self>) {
        if !self.plugin_runtime.wanted.contains(&EventKind::ActionPerformed) {
            return;
        }
        let title = cx
            .try_global::<ActionRegistry>()
            .and_then(|registry| registry.get(id))
            .map_or_else(|| id.to_owned(), |action| action.meta.title.clone());
        let shortcut = cx.try_global::<Keymap>().and_then(|keymap| keymap.0.shortcut_label(id, is_mac_host()));
        let performed = ActionPerformed { action: id.to_owned(), title, source: plugin_source(source), shortcut };
        self.post_plugin_event(WorkbenchEvent::ActionPerformed(performed), cx);
    }
}

/// Answers the host's request `request_id` of window `label`.
pub(super) fn answer_plugin_command(label: &str, request_id: &str, answer: CommandAnswer, cx: &App) {
    let allowed = cx
        .try_global::<Backend>()
        .is_some_and(|backend| backend.host.authorize_command(label, "plugin_answer_command").is_ok());
    let Some(host) = plugin_host(cx).filter(|_| allowed) else {
        tracing::warn!("the plugin request {request_id} is not answered in this window");
        return;
    };
    if !host.answer_command(label, request_id, answer) {
        tracing::debug!("the plugin request {request_id} no longer waits");
    }
}

/// Kills terminal `id` off the main thread; one that already ended is left alone.
fn kill_terminal(id: TerminalId, executor: &gpui_kit::BackgroundExecutor, host: Option<Arc<ide_host::Host>>) {
    let Some(host) = host else { return };
    executor
        .spawn(async move {
            if let Err(error) = host.terminals.kill(id) {
                tracing::debug!("the log tab's terminal had ended: {}", error.message);
            }
        })
        .detach();
}

/// The wire name of a plugin event's kind (`viewMessage`, `decorationsChanged`…), without its fields.
fn plugin_event_kind(event: &PluginEvent) -> String {
    let wire = serde_json::to_value(event).ok();
    wire.as_ref().and_then(|wire| wire.get("type")).and_then(Value::as_str).unwrap_or("unknown").to_owned()
}

#[cfg(test)]
mod tests {
    use super::super::notification_balloons::EntryAction;
    use super::super::plugins_window::test_support::{link, plugin_copy, start_with_plugins as start, trusted_project};
    #[cfg(unix)]
    use super::super::plugins_window::test_support::{program, programs_in};
    use super::super::tests::{git_in, init_repo, notification_titles, run_action, wait_long as wait};
    use super::*;
    use crate::platform::actions::is_action_enabled;
    use crate::platform::context::{ContextValue, window_context};
    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext};
    use std::path::Path;

    mod proofs6_plugin_runtime;
    mod proofs9_status_snapshot;

    const LABEL: &str = "w1";

    /// A trusted project of `files` (path, text), open in an active window.
    fn project<'a>(
        cx: &'a mut TestAppContext,
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, PathBuf, Entity<Workbench>, &'a mut VisualTestContext) {
        trusted_project(cx, LABEL, files)
    }

    /// Runs `command` of `plugin` for the window off the main thread, as a command the user ran
    /// there.
    fn run_in_window(
        cx: &mut VisualTestContext,
        root: &Path,
        plugin: &str,
        command: &str,
        arg: Option<&str>,
    ) -> std::thread::JoinHandle<IdeResult<Option<String>>> {
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        let invocation = Invocation {
            command_chain: Vec::new(),
            client: Some(LABEL.to_owned()),
            workspace_root: Some(root.to_path_buf()),
            editor: None,
            selected_paths: Vec::new(),
            global_settings: false,
        };
        let (plugin, command, arg) = (plugin.to_owned(), command.to_owned(), arg.map(str::to_owned));
        std::thread::spawn(move || host.run_command(&plugin, &command, arg.as_deref(), invocation))
    }

    fn status_texts(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Vec<String> {
        let bar = workbench.read_with(cx, |w, _| w.status_bar().clone());
        cx.update(|window, cx| bar.read(cx).texts(window, cx))
    }

    fn enabled(id: &str, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| {
            let action = cx.global::<ActionRegistry>().get(id).expect("registered");
            is_action_enabled(&action.meta, &window_context(window, cx))
        })
    }

    fn context(key: &str, cx: &mut VisualTestContext) -> Option<ContextValue> {
        cx.update(|window, cx| window_context(window, cx).get(key).cloned())
    }

    /// The Hello fixture's manifest with its Word Count command on only in a project holding a
    /// `MODULE.bazel` file.
    fn word_count_needs_module_file(text: &str) -> String {
        text.replace(
            "title = \"Hello: Word Count of Active File\"",
            "title = \"Hello: Word Count of Active File\"\nwhen = \"workspaceContains:MODULE.bazel\"",
        )
    }

    // src/features/plugins/__tests__/workspaceContains.test.ts › workspaceContains
    /// A plugin command whose `when` names `workspaceContains:MODULE.bazel` is off in a project
    /// without that file, comes on once it is created and goes off when it is deleted.
    #[gpui_kit::test]
    async fn re_evaluates_when_the_file_is_created_and_deleted(cx: &mut TestAppContext) {
        let home = start(cx);
        let folder = plugin_copy(home.path(), "hello", word_count_needs_module_file);
        let id = link(cx, &folder, &[], "word-count");
        let (_folder, root, _workbench, cx) = project(cx, &[("src/notes.txt", "one two\n")]);
        const KEY: &str = "workspaceContains:MODULE.bazel";
        let action = plugin_action_id(&id, "word-count");
        wait(cx, "the key is answered", |cx| context(KEY, cx) == Some(ContextValue::Bool(false)));
        assert!(!enabled(&action, cx), "off without MODULE.bazel");
        std::fs::write(root.join("MODULE.bazel"), "module(name = \"x\")\n").expect("the file is written");
        wait(cx, "the command comes on", |cx| enabled(&action, cx));
        std::fs::remove_file(root.join("MODULE.bazel")).expect("the file is removed");
        wait(cx, "the command goes off", |cx| !enabled(&action, cx));
    }

    // src/features/plugins/__tests__/plugins.test.tsx › bridge events
    /// A plugin's code sets a global status item, overrides it in this window and removes the
    /// override: the status bar shows the window's text over the global one, then the global one.
    #[gpui_kit::test]
    async fn keeps_a_window_status_override_above_global_updates_and_falls_back_on_removal(cx: &mut TestAppContext) {
        let home = start(cx);
        let folder = plugin_copy(home.path(), "api14", str::to_owned);
        let id = link(cx, &folder, &[], "global-status");
        let (_folder, _root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
        let shows = |text: &str, cx: &mut VisualTestContext| status_texts(&workbench, cx).iter().any(|t| t == text);
        run_action(cx, &plugin_action_id(&id, "global-status"), Some(Value::from("global")));
        wait(cx, "the global item shows", |cx| shows("global", cx));
        run_action(cx, &plugin_action_id(&id, "window-status"), Some(Value::from("window")));
        wait(cx, "the window's item shows", |cx| shows("window", cx));
        run_action(cx, &plugin_action_id(&id, "global-status"), Some(Value::from("updated")));
        // The global update arrives, and the window's item stays in front of it.
        wait(cx, "the update is listed", |cx| {
            cx.update(|_, cx| {
                cx.global::<Plugins>().host.status_items().iter().any(|(_, _, item)| item.text == "updated")
            })
        });
        cx.run_until_parked();
        assert!(shows("window", cx));
        assert!(!shows("updated", cx));
        run_action(cx, &plugin_action_id(&id, "remove-window-status"), None);
        wait(cx, "the global item is back", |cx| shows("updated", cx));
        assert!(!shows("window", cx));
    }

    /// A plugin's declared status item shows before its code runs, named after the plugin in the
    /// status bar's menu; a click runs its command in the plugin, whose notification appears.
    #[gpui_kit::test]
    async fn a_declared_status_item_shows_and_a_click_runs_its_plugin_command(cx: &mut TestAppContext) {
        let home = start(cx);
        let folder = plugin_copy(home.path(), "hello", str::to_owned);
        link(cx, &folder, &[], "word-count");
        let (_folder, root, workbench, cx) = project(cx, &[("notes.txt", "one two three\nfour\n")]);
        wait(cx, "the declared item shows", |cx| status_texts(&workbench, cx).iter().any(|t| t == "Hello"));
        let spec = workbench.read_with(cx, |w, cx| w.status_bar().read(cx).model().spec("plugin.hello/hello").cloned());
        assert_eq!(spec.and_then(|spec| spec.title).as_deref(), Some("Hello: Word count of the active file"));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        wait(cx, "the file opens", |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.run_until_parked();
        let bounds = cx.debug_bounds("status:plugin.hello/hello").expect("the item is drawn");
        cx.simulate_click(bounds.center(), Modifiers::none());
        wait(cx, "the plugin's notification", |cx| {
            notification_titles(&workbench, cx).iter().any(|title| title == "Word count: notes.txt")
        });
    }

    /// A plugin's cancellable task is listed with its title while it runs; Cancel shows
    /// "Cancelling…", stops the program the plugin waits on, and the task leaves the list.
    #[cfg(unix)]
    #[gpui_kit::test]
    async fn a_plugins_task_is_listed_while_it_runs_and_cancel_stops_it(cx: &mut TestAppContext) {
        let home = start(cx);
        let bin = home.path().join("bin");
        program(&bin, "checkstyle", "#!/bin/sh\nexec /bin/sleep 600\n");
        programs_in(&bin, cx);
        let folder = plugin_copy(home.path(), "checkstyle", str::to_owned);
        let id = link(cx, &folder, &[], "check");
        let (_folder, root, workbench, cx) = project(cx, &[("src/Main.java", "class Main {}\n")]);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("src/Main.java"), window, cx));
        wait(cx, "the file opens", |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        // The project's own work (indexing) is over first, so the plugin's task is the one the
        // status bar names.
        wait(cx, "the project's tasks end", |cx| workbench.read_with(cx, |w, _| w.tasks().tasks().is_empty()));
        run_action(cx, &plugin_action_id(&id, "check"), None);
        let listed = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| {
                w.tasks().tasks().iter().find(|task| task.title == "Checkstyle: Main.java").cloned()
            })
        };
        wait(cx, "the task is listed", |cx| listed(cx).is_some());
        let task = listed(cx).expect("the task");
        assert!(super::super::tasks::can_cancel(&task), "the plugin made it cancellable");
        assert_eq!(workbench.read_with(cx, |w, _| w.task_status()).as_deref(), Some("Checkstyle: Main.java"));
        workbench.update(cx, |w, cx| w.cancel_background_task(&task.id, cx));
        assert_eq!(listed(cx).and_then(|task| task.message).as_deref(), Some("Cancelling…"));
        wait(cx, "the task ends", |cx| listed(cx).is_none());
    }

    // src/features/plugins/__tests__/plugins.test.tsx › bridge events
    /// A plugin's notification shows its three buttons; pressing the second runs the plugin's
    /// command with that button's argument, which sets the context key it names in this window.
    #[cfg(unix)]
    #[gpui_kit::test]
    async fn runs_notification_buttons_as_commands_of_the_publishing_plugin_with_their_arguments(
        cx: &mut TestAppContext,
    ) {
        let home = start(cx);
        let bin = home.path().join("bin");
        program(&bin, "jobs", "#!/bin/sh\nexit 0\n");
        programs_in(&bin, cx);
        let folder = plugin_copy(home.path(), "jobs", str::to_owned);
        let id = link(cx, &folder, &["project.read"], "notify-actions");
        let (_folder, _root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
        run_action(cx, &plugin_action_id(&id, "notify-actions"), None);
        let notice = |cx: &mut VisualTestContext| {
            workbench
                .read_with(cx, |w, _| w.notifications().items().iter().find(|n| n.title == "Choose an action").cloned())
        };
        wait(cx, "the notification", |cx| notice(cx).is_some());
        let notice = notice(cx).expect("the notification");
        assert_eq!(notice.body.as_deref(), Some("A plugin notification"));
        assert!(notice.verbatim);
        let labels: Vec<&str> = notice.actions.iter().map(|action| action.label.as_str()).collect();
        assert_eq!(labels, ["Action 1", "Action 2", "Action 3"]);
        let press = EntryAction::Own(notice.actions[1].clone());
        workbench.update_in(cx, |w, window, cx| w.run_notification_action(press, window, cx));
        wait(cx, "the plugin set its key", |cx| {
            context("plugin.jobs.notification-action", cx) == Some(ContextValue::from("2\nvalue"))
        });
    }

    /// A plugin's view asks to open a project file at a line and column: it opens there. The
    /// plugin's text reaches the clipboard and its link the browser.
    #[cfg(unix)]
    #[gpui_kit::test]
    async fn a_plugin_opens_a_file_at_a_position_copies_text_and_opens_a_url(cx: &mut TestAppContext) {
        let home = start(cx);
        let bin = home.path().join("bin");
        program(&bin, "jobs", "#!/bin/sh\nexit 0\n");
        programs_in(&bin, cx);
        let folder = plugin_copy(home.path(), "jobs", str::to_owned);
        link(cx, &folder, &["project.read", "clipboard"], "follow");
        let (_folder, root, workbench, cx) = project(cx, &[("jobs.log", "build\ndeploy\nfailed\n")]);
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        let invocation = Invocation {
            command_chain: Vec::new(),
            client: Some(LABEL.to_owned()),
            workspace_root: Some(root.clone()),
            editor: None,
            selected_paths: Vec::new(),
            global_settings: false,
        };
        let pressed = std::thread::spawn(move || {
            host.view_event("jobs", "jobs", ide_plugin::ViewEvent::Button { id: "log".to_owned() }, invocation)
        });
        pressed.join().expect("the view event ran").expect("the plugin opens the file");
        wait(cx, "the file opens", |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("jobs.log")))
        });
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        wait(cx, "the caret is placed", |cx| editor.read_with(cx, |e, _| e.caret_position()) == (2, 4));

        let copied = run_in_window(cx, &root, "jobs", "copy", Some("https://example.test/a"));
        copied.join().expect("the command ran").expect("the plugin copies");
        wait(cx, "the text is on the clipboard", |cx| {
            cx.read_from_clipboard().and_then(|item| item.text()).as_deref() == Some("https://example.test/a")
        });
        let opened = run_in_window(cx, &root, "jobs", "open-url", Some("https://example.test/b"));
        opened.join().expect("the command ran").expect("the plugin opens the link");
        wait(cx, "the link opens", |cx| cx.opened_url().as_deref() == Some("https://example.test/b"));
    }

    /// The plugin's code asks for a name while its command waits: the question shows with the
    /// text it suggests; the answer typed and confirmed is what the command gets back.
    #[cfg(unix)]
    #[gpui_kit::test]
    async fn a_plugins_text_question_is_answered_and_the_command_gets_the_text(cx: &mut TestAppContext) {
        let home = start(cx);
        let bin = home.path().join("bin");
        program(&bin, "jobs", "#!/bin/sh\nexit 0\n");
        programs_in(&bin, cx);
        let folder = plugin_copy(home.path(), "jobs", str::to_owned);
        link(cx, &folder, &[], "follow");
        let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
        let running = run_in_window(cx, &root, "jobs", "input", Some("Main"));
        wait(cx, "the question shows", |cx| workbench.read_with(cx, |w, _| w.plugin_prompt().is_some()));
        let prompt = workbench.read_with(cx, |w, _| w.plugin_prompt().cloned()).expect("a question");
        assert_eq!(
            prompt.read_with(cx, |p, cx| (p.title().to_owned(), p.text(cx))),
            ("Name".to_owned(), Some("Main".to_owned()))
        );
        let field = prompt.read_with(cx, |p, _| p.field().cloned()).expect("a one-line field");
        assert!(cx.update(|window, cx| field.focus_handle(cx).is_focused(window)), "the field has the keyboard");
        field.update(cx, |field, cx| field.set_text(" Other ", cx));
        cx.simulate_keystrokes("enter");
        wait(cx, "the command ends", |_| running.is_finished());
        assert_eq!(running.join().expect("the command ran").expect("it answers").as_deref(), Some(" Other "));
        assert!(workbench.read_with(cx, |w, _| w.plugin_prompt().is_none()));
    }

    // src/features/plugins/__tests__/plugins.test.tsx › plugin prompts
    /// Escape dismisses a pick: the command hears "none". A question whose plugin is turned off
    /// meanwhile closes without an answer.
    #[cfg(unix)]
    #[gpui_kit::test]
    async fn closes_a_question_the_plugin_gave_up_on_without_answering(cx: &mut TestAppContext) {
        let home = start(cx);
        let bin = home.path().join("bin");
        program(&bin, "jobs", "#!/bin/sh\nexit 0\n");
        programs_in(&bin, cx);
        let folder = plugin_copy(home.path(), "jobs", str::to_owned);
        link(cx, &folder, &[], "follow");
        let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
        let running = run_in_window(cx, &root, "jobs", "pick-many", None);
        wait(cx, "the pick shows", |cx| workbench.read_with(cx, |w, _| w.plugin_prompt().is_some()));
        let prompt = workbench.read_with(cx, |w, _| w.plugin_prompt().cloned()).expect("a question");
        assert_eq!(prompt.read_with(cx, |p, _| p.rows()), ["Alpha\tfirst", "Beta", "Gamma"]);
        cx.simulate_keystrokes("escape");
        wait(cx, "the command ends", |_| running.is_finished());
        assert_eq!(running.join().expect("the command ran").expect("it answers").as_deref(), Some("none"));

        let running = run_in_window(cx, &root, "jobs", "pick", None);
        wait(cx, "the next pick shows", |cx| workbench.read_with(cx, |w, _| w.plugin_prompt().is_some()));
        cx.update(|_, cx| cx.global::<Plugins>().host.set_enabled("jobs", false)).expect("the plugin turns off");
        wait(cx, "the question closes", |cx| workbench.read_with(cx, |w, _| w.plugin_prompt().is_none()));
        wait(cx, "the command ends", |_| running.is_finished());
        assert!(running.join().expect("the command ran").is_err(), "the command was cancelled");
    }

    /// The plugin's branch item follows the window: the focused editor's project shows its branch,
    /// and a checkout of another branch (a `vcs-changed` the window reports) shows that one.
    #[gpui_kit::test]
    async fn the_focused_editor_and_a_checkout_reach_the_plugin_which_shows_the_branch(cx: &mut TestAppContext) {
        let home = start(cx);
        let folder = plugin_copy(home.path(), "git-toolbox", str::to_owned);
        link(cx, &folder, &[], "refresh");
        let repo = tempfile::tempdir().expect("a temporary folder");
        let root = repo.path().canonicalize().expect("the folder exists");
        init_repo(&root);
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new(LABEL, window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
        wait(cx, "the project opens", |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("a.txt"), window, cx));
        wait(cx, "the plugin shows the branch", |cx| status_texts(&workbench, cx).iter().any(|t| t == "main"));
        git_in(&root, &["checkout", "-q", "-b", "topic"]);
        wait(cx, "the plugin shows the new branch", |cx| status_texts(&workbench, cx).iter().any(|t| t == "topic"));
    }

    /// A setting of the plugin changes: the plugin hears `settings-changed` with the key that
    /// changed.
    #[gpui_kit::test]
    async fn a_changed_plugin_setting_reaches_the_plugin_as_settings_changed(cx: &mut TestAppContext) {
        use ide_workspace::settings::SettingsScope;
        let home = start(cx);
        let folder = plugin_copy(home.path(), "api14", str::to_owned);
        let id = link(cx, &folder, &["events"], "events");
        let (_folder, root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
        // The plugin runs once, so it is live when the event comes.
        run_in_window(cx, &root, &id, "events", None).join().expect("the command ran").expect("it answers");
        // The host keeps the settings the window told it as the plugin's baseline; a change made
        // before that would be the baseline itself.
        wait(cx, "the settings went out once", |cx| workbench.read_with(cx, |w, _| w.plugin_settings_told()));
        cx.update(|_, cx| {
            crate::platform::settings::set(
                LABEL,
                SettingsScope::Global,
                "plugins.api14.enabled",
                &Value::Bool(true),
                cx,
            )
        })
        .expect("the setting is stored");
        let received = |cx: &mut VisualTestContext| {
            let events = run_in_window(cx, &root, "api14", "events", None).join().expect("the command ran");
            let events: Value =
                events.ok().flatten().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
            events.as_array().is_some_and(|events| {
                events.iter().any(|e| e["kind"] == "settings-changed" && e["data"] == "[\"enabled\"]")
            })
        };
        wait(cx, "the plugin heard the change", received);
    }

    /// The plugin's code opens a log tab: the editor area shows it with the program's lines; its
    /// Verbose switch restarts the program with its argument, and Clear empties it.
    #[cfg(unix)]
    #[gpui_kit::test]
    async fn starts_a_tab_opened_by_plugin_code_by_its_key_with_the_toggles_that_are_on(cx: &mut TestAppContext) {
        let home = start(cx);
        programs_in(Path::new("/bin"), cx);
        let folder = plugin_copy(home.path(), "web-log", str::to_owned);
        let id = link(cx, &folder, &["process:bash"], "code");
        let (_folder, _root, workbench, cx) = project(cx, &[("a.txt", "a\n")]);
        run_action(cx, &plugin_action_id(&id, "code"), None);
        wait(cx, "the log tab opens", |cx| {
            workbench
                .read_with(cx, |w, _| w.tab_titles().iter().any(|tab| tab.name == "Probe logs (code)" && tab.active))
        });
        let tab = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.plugin_log_tabs().into_iter().next().map(|(_, view)| view))
        };
        wait(cx, "the tab is shown", |cx| tab(cx).is_some());
        let view = tab(cx).expect("the log tab");
        let has = |text: &'static str| {
            let view = view.clone();
            move |cx: &mut VisualTestContext| {
                view.read_with(cx, |v, _| v.lines().iter().any(|line| line.contains(text)))
            }
        };
        wait(cx, "the program prints", has("INFO probe line 1 — Unicode 日志"));
        assert!(!has("DEBUG verbose output enabled")(cx));
        run_action(cx, TOGGLE_ARGUMENT, Some(serde_json::json!({ "id": "verbose", "checked": true })));
        assert_eq!(view.read_with(cx, |v, _| v.toggles()), [("verbose".to_owned(), "Verbose".to_owned(), true)]);
        wait(cx, "the program restarts verbose", has("DEBUG verbose output enabled"));
        run_action(cx, "plugins.log.clear", None);
        assert_eq!(view.read_with(cx, |v, _| v.text().to_owned()), "");
        assert!(view.read_with(cx, |v, _| v.wraps()));
        run_action(cx, "plugins.log.wrap", None);
        assert!(!view.read_with(cx, |v, _| v.wraps()));
    }
}
