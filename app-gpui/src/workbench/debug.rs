//! The debugger of a project window: its [`DebugDriver`], the Debug tool window with the debug
//! console, the Breakpoints dialog, breakpoints and the execution line in the editors, and the
//! `debug.*` actions with `run.debug` / `run.debugConfig`.
//!
//! The Breakpoints dialog is a layer of its own over the window (as Edit Configurations is): only
//! its own buttons and Escape close it.

use super::services_window::on_workbench;
use super::{Panel, Workbench, for_window};
use crate::debugger::actions::{ActiveEditor, Dispatch, ViewCommand};
use crate::debugger::breakpoints_view::{BreakpointsDialog, BreakpointsDialogEvent};
use crate::debugger::driver::{DebugDriver, DebugDriverEvent};
use crate::debugger::editor::breakpoint_gutter::{
    BreakpointLook, ClickOutcome, GutterButton, GutterEditor, GutterPress, PressOutcome, build_markers,
};
use crate::debugger::effects::{DEBUG_TOOL_WINDOW, Effect, run_notification};
use crate::debugger::focused_view::{DebugView, FRAMES_FOCUS, VARIABLES_FOCUS};
use crate::debugger::frames_view::{FramesView, FramesViewEvent};
use crate::debugger::state::{CloseTab, Ctx, DebugState};
use crate::debugger::variables_view::{VariablesView, VariablesViewEvent};
use crate::debugger::window_view::{DebugWindowEvent, DebugWindowView};
use crate::editor::buffer;
use crate::editor::marks::{Mark, MarkKind, MarkLayer};
use crate::editor::view::{Editor, EditorEvent};
use crate::platform::actions::{ActionRegistry, ActionSource, execute_action_from};
use crate::platform::context::set_context;
use crate::platform::i18n::{Localization, Param};
use crate::run::store::ChooserMode;
use crate::theme;
use crate::workbench::notifications::NotificationLevel;
use futures::channel::oneshot;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, EntityId, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Subscription, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use ropey::Rope;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

/// The transitions' surroundings, from the fields of a debugger and its host.
macro_rules! ctx {
    ($debugger:ident, $host:ident) => {
        Ctx { breakpoints: &mut $debugger.breakpoints, env: $host.env, i18n: &mut *$host.i18n }
    };
}

/// The `debug.*` actions this window carries out on its debugger. Evaluate Expression, Attach to
/// Process, Smart Step Into, Quick Evaluate and the console history have no view here yet.
const DEBUG_ACTIONS: [&str; 28] = [
    "debug.startConfig",
    "debug.stop",
    "debug.reloadChangedClasses",
    "debug.cancelHotSwap",
    "debug.rerun",
    "debug.resume",
    "debug.pause",
    "debug.hotReload",
    "debug.hotRestart",
    "debug.stepOver",
    "debug.stepInto",
    "debug.stepOut",
    "debug.forceStepOver",
    "debug.forceStepInto",
    "debug.showExecutionPoint",
    "debug.resetFrame",
    "debug.setValue",
    "debug.jumpToSource",
    "debug.runToCursor",
    "debug.forceRunToCursor",
    "debug.toggleBreakpoint",
    "debug.toggleTemporaryBreakpoint",
    "debug.toggleMethodBreakpoint",
    "debug.toggleFieldWatchpoint",
    "debug.viewBreakpoints",
    "debug.removeAllBreakpoints",
    "debug.showToolWindow",
    "debug.muteBreakpoints",
];

/// The debugger of the open project.
pub(super) struct DebugSlot {
    driver: Entity<DebugDriver>,
    /// The Debug tool window, once it was shown.
    window: Option<DebugWindowSlot>,
    /// The Breakpoints dialog, while it is open.
    breakpoints: Option<BreakpointsWindow>,
    /// What tells the window of a press in each editor's gutter, by editor.
    gutters: HashMap<EntityId, Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// The Debug tool window's view and panes, and what the window listens to them for.
struct DebugWindowSlot {
    view: Entity<DebugWindowView>,
    frames: Entity<FramesView>,
    variables: Entity<VariablesView>,
    _subscriptions: Vec<Subscription>,
}

/// The Breakpoints dialog over the window.
struct BreakpointsWindow {
    dialog: Entity<BreakpointsDialog>,
    /// What had the keyboard before the dialog opened.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

/// What the debugger reads of an editor, owned so the editor is free again while it decides.
struct EditorSnapshot {
    path: String,
    language: String,
    rope: Rope,
    selection: (u32, u32),
    head: u32,
}

impl EditorSnapshot {
    fn of(editor: &Entity<Editor>, cx: &App) -> Self {
        let editor = editor.read(cx);
        let selection = editor.selection();
        Self {
            path: editor.path().to_string_lossy().into_owned(),
            language: editor.language().as_str().to_owned(),
            rope: editor.rope().clone(),
            selection: (selection.start, selection.end),
            head: editor.caret(),
        }
    }

    fn active(&self) -> ActiveEditor<'_> {
        ActiveEditor {
            path: &self.path,
            language: &self.language,
            doc: &self.rope,
            selection: self.selection,
            head: self.head,
            comments: &[],
        }
    }

    fn gutter(&self) -> GutterEditor<'_> {
        GutterEditor { path: &self.path, language: &self.language, doc: &self.rope, comments: &[] }
    }
}

/// Runs `act` with the workbench of the action's window.
fn on_debug(
    window: &mut Window,
    cx: &mut App,
    act: impl FnOnce(&mut Workbench, &mut Window, &mut Context<Workbench>),
) -> anyhow::Result<()> {
    on_workbench(window, cx, act)
}

/// Registers the debugger's actions and the Run feature's Debug entries.
pub(super) fn register_actions(registry: &mut ActionRegistry) {
    for id in DEBUG_ACTIONS {
        if id == "debug.muteBreakpoints" {
            registry.handle_toggle(
                id,
                |window, cx| {
                    let workbench = for_window(window, cx);
                    let driver = workbench.as_ref().and_then(|workbench| workbench.read(cx).debug_driver().cloned());
                    driver.is_some_and(|driver| driver.read(cx).debugger().read(cx).breakpoints_muted())
                },
                move |call, window, cx| {
                    let arg = call.arg.cloned();
                    on_debug(window, cx, move |workbench, window, cx| {
                        workbench.run_debug_action(id, arg.as_ref(), window, cx);
                    })
                },
            );
            continue;
        }
        registry.handle(id, move |call, window, cx| {
            let arg = call.arg.cloned();
            on_debug(window, cx, move |workbench, window, cx| workbench.run_debug_action(id, arg.as_ref(), window, cx))
        });
    }
    registry.handle("run.debug", |_, window, cx| {
        on_debug(window, cx, |workbench, window, cx| workbench.debug_selected(window, cx))
    });
    registry.handle("run.debugConfig", |call, window, cx| {
        let arg = call.arg.cloned();
        on_debug(window, cx, move |workbench, window, cx| workbench.debug_config(arg, window, cx))
    });
}

impl Workbench {
    /// The debugger of the window's project.
    pub fn debug_driver(&self) -> Option<&Entity<DebugDriver>> {
        self.debug.as_ref().map(|slot| &slot.driver)
    }

    /// The Debug tool window's view, once it was shown.
    pub fn debug_window_view(&self) -> Option<&Entity<DebugWindowView>> {
        self.debug.as_ref()?.window.as_ref().map(|window| &window.view)
    }

    /// The Breakpoints dialog, while it is open.
    pub fn breakpoints_dialog(&self) -> Option<&Entity<BreakpointsDialog>> {
        self.debug.as_ref()?.breakpoints.as_ref().map(|open| &open.dialog)
    }

    // ---------------------------------------------------------------------------------------------
    // The driver

    /// The debugger of a project just opened at `root`: its breakpoints and watches are read, and
    /// the window follows what it asks for and what changes in it.
    pub(super) fn mount_debugger(&mut self, root: &str, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        // The driver asks to save while the workbench may be updating (a Start from an action): the
        // documents are read once that update is over.
        let save_all: crate::run::driver::SaveAll = std::rc::Rc::new(move |cx: &mut App| {
            use futures::FutureExt as _;
            let weak = weak.clone();
            cx.spawn(async move |cx| match weak.update(cx, |workbench, cx| workbench.save_all(cx)) {
                Ok(saving) => saving.await,
                // The window is gone: nothing of it is left to save.
                Err(_) => Ok(()),
            })
            .boxed_local()
        });
        let label = self.label.clone();
        let driver = cx.new(|cx| DebugDriver::new(&label, cx).with_save_all(save_all));
        let debugger = driver.read(cx).debugger().clone();
        let subscriptions = vec![
            cx.subscribe_in(&driver, window, Self::debug_driver_event),
            cx.observe_in(&debugger, window, |workbench, _, window, cx| workbench.debugger_changed(window, cx)),
        ];
        let root = root.to_owned();
        driver.update(cx, |driver, cx| driver.load(Some(&root), cx));
        self.debug = Some(DebugSlot {
            driver,
            window: None,
            breakpoints: None,
            gutters: HashMap::new(),
            _subscriptions: subscriptions,
        });
    }

    /// The project goes: its debug sessions stop and the debugger goes with it.
    pub(super) fn reset_debugger(&mut self, cx: &mut Context<Self>) {
        if let Some(slot) = self.debug.take() {
            slot.driver.update(cx, |driver, cx| driver.reset(cx));
        }
    }

    /// A debug event of the backend for this window.
    pub(super) fn debug_backend_event(&mut self, event: &ide_core::IdeEvent, cx: &mut Context<Self>) {
        if let Some(driver) = self.debug_driver().cloned() {
            driver.update(cx, |driver, cx| driver.backend_event(event, cx));
        }
    }

    /// A notification button: true when it was one of the debugger's.
    pub(super) fn debug_notification_command(&mut self, command: &str, cx: &mut Context<Self>) -> bool {
        let Some(driver) = self.debug_driver().cloned() else { return false };
        driver.update(cx, |driver, cx| driver.notification_command(command, cx))
    }

    /// Carries out what the debug driver asks of the window.
    fn debug_driver_event(
        &mut self,
        _: &Entity<DebugDriver>,
        event: &DebugDriverEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            DebugDriverEvent::Notify(request) => self.notify(request, cx),
            DebugDriverEvent::ExpireNotification(key) => {
                self.notifications.expire_notification(&key);
                self.sync_notifications(cx);
            }
            DebugDriverEvent::ShowToolWindow { focus } => {
                self.with_layout(window, cx, |layout| layout.show(DEBUG_TOOL_WINDOW, focus));
            }
            DebugDriverEvent::HideToolWindow => self.with_layout(window, cx, |layout| layout.hide(DEBUG_TOOL_WINDOW)),
            DebugDriverEvent::RunAction { id, arg } => window.defer(cx, move |window, cx| {
                if !execute_action_from(ActionSource::Other, &id, arg.as_ref(), window, cx) {
                    tracing::info!("{id} is not available in this window");
                }
            }),
            DebugDriverEvent::OpenFile { path, line, column, focus } => {
                self.open_debug_file(path, line, column, focus, window, cx);
            }
            DebugDriverEvent::StatusInfo(text) => {
                let mut request = run_notification(NotificationLevel::Info, text);
                request.key = Some("debug.status".to_owned());
                self.notify(request, cx);
            }
            DebugDriverEvent::PluginSessions(events) => self.plugin_debug_sessions(events, cx),
            DebugDriverEvent::FocusAppWindow => window.activate_window(),
            DebugDriverEvent::ValueSet { request, result } => {
                if let Some(variables) = self.debug_variables_view() {
                    variables.update(cx, |variables, cx| variables.value_set(request, result, window, cx));
                }
            }
        }
    }

    /// Opens a frame's (or a breakpoint's) source at its line. The editor takes the keyboard only
    /// when `focus` says so; otherwise the keyboard goes back to where it was.
    fn open_debug_file(
        &mut self,
        path: String,
        line: u32,
        column: u32,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restore = if focus { None } else { window.focused(cx) };
        self.open_project_file_at(PathBuf::from(path), line, column, window, cx);
        if let Some(restore) = restore {
            window.focus(&restore, cx);
        }
    }

    /// Something in the debugger changed: the editors' marks, the Breakpoints dialog and the context
    /// keys follow it.
    fn debugger_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_debug_marks(window, cx);
        self.sync_breakpoints_dialog(window, cx);
        cx.notify();
    }

    /// Publishes the context keys of the debugger: its sessions' state and which of its views has
    /// the keyboard.
    pub(super) fn publish_debug_context(&self, window: &mut Window, cx: &mut App) {
        let state = self.debug_driver().map(|driver| {
            let debugger = driver.read(cx).debugger().read(cx);
            (debugger.state.published().context, debugger.focused.current())
        });
        let (context, focused) = state.unwrap_or_default();
        for (key, value) in context.entries() {
            set_context(window, cx, key, value);
        }
        let view = focused.map(|commands| commands.view);
        set_context(window, cx, FRAMES_FOCUS, view == Some(DebugView::Frames));
        set_context(window, cx, VARIABLES_FOCUS, view == Some(DebugView::Variables));
    }

    // ---------------------------------------------------------------------------------------------
    // Actions

    /// Runs the debugger action `id` with the editor that has the focus, and does what only a view
    /// of the window can.
    pub fn run_debug_action(&mut self, id: &str, arg: Option<&Value>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(driver) = self.debug_driver().cloned() else { return };
        let editor = self.editor().map(|editor| EditorSnapshot::of(editor, cx));
        let active = editor.as_ref().map(EditorSnapshot::active);
        let dispatch = driver.update(cx, |driver, cx| driver.run_action(id, arg, active.as_ref(), cx));
        match dispatch {
            Dispatch::View(ViewCommand::SetValue) => {
                if let Some(variables) = self.debug_variables_view() {
                    variables.update(cx, |variables, cx| variables.begin_set_value(window, cx));
                }
            }
            Dispatch::View(ViewCommand::JumpToSource) => {
                let focused = driver.read(cx).debugger().read(cx).focused.current().map(|commands| commands.view);
                match (focused, self.debug.as_ref().and_then(|slot| slot.window.as_ref())) {
                    (Some(DebugView::Frames), Some(open)) => {
                        open.frames.update(cx, |frames, cx| frames.jump_to_source(cx));
                    }
                    (Some(DebugView::Variables), Some(open)) => {
                        open.variables.update(cx, |variables, cx| variables.jump_to_source(cx));
                    }
                    _ => {}
                }
            }
            Dispatch::View(ViewCommand::BrowseConsoleHistory) | Dispatch::QuickEvaluate(_) => {
                tracing::debug!("{id} has no view in this window");
            }
            Dispatch::Done | Dispatch::Unknown => {}
        }
        // The choice between several debuggable configurations is the run configuration chooser's.
        let chose = driver.update(cx, |driver, cx| {
            driver.debugger().update(cx, |debugger, _| debugger.overlays.chooser.take().is_some())
        });
        if chose {
            self.choose_config(ChooserMode::Debug, window, cx);
        }
    }

    /// Does `then` in the window once the run driver has listed the configurations.
    fn after_configs_listed(
        &mut self,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = self.run_driver().cloned() else { return };
        let (sender, listed) = oneshot::channel();
        run.update(cx, |run, cx| {
            run.when_loaded(
                move |_, _| {
                    // The receiver is gone only with the window.
                    let _ = sender.send(());
                },
                cx,
            );
        });
        cx.spawn_in(window, async move |workbench, cx| {
            // The project closed before its configurations were listed: there is nothing to debug.
            if listed.await.is_err() {
                return;
            }
            let _ = workbench.update_in(cx, then);
        })
        .detach();
    }

    /// Run › Debug: the selected configuration, once the configurations are listed; the active
    /// file's context when "Current File" is selected; the chooser when nothing is.
    pub fn debug_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.after_configs_listed(
            |workbench, window, cx| {
                let Some(run) = workbench.run_driver().cloned() else { return };
                let (selected, current_file) = {
                    let store = run.read(cx).store().read(cx);
                    (store.selected_config().cloned(), store.current_file_selected())
                };
                if current_file {
                    return window.defer(cx, |window, cx| {
                        if !execute_action_from(ActionSource::Other, "run.debugContext", None, window, cx) {
                            tracing::info!("run.debugContext is not available in this window");
                        }
                    });
                }
                match selected {
                    Some(config) => workbench.debug_config(serde_json::to_value(config).ok(), window, cx),
                    None => workbench.choose_config(ChooserMode::Debug, window, cx),
                }
            },
            window,
            cx,
        );
    }

    /// Debug Configuration: the configuration `arg` names (an id, a configuration, or `{ config,
    /// show }`) is started under the debugger once the configurations are listed.
    pub fn debug_config(&mut self, arg: Option<Value>, window: &mut Window, cx: &mut Context<Self>) {
        if self.debug_driver().is_none() {
            return;
        }
        let show = arg.as_ref().and_then(|arg| arg.get("show")).and_then(Value::as_bool);
        self.after_configs_listed(
            move |workbench, window, cx| {
                let Some(run) = workbench.run_driver().cloned() else { return };
                let Some(config) = run.update(cx, |run, cx| run.config_from_arg(arg.as_ref(), cx)) else { return };
                run.update(cx, |run, cx| run.mark_recent(&config.id, cx));
                let mut start = json!({ "config": serde_json::to_value(&config).unwrap_or(Value::Null) });
                if let Some(show) = show {
                    start["show"] = Value::Bool(show);
                }
                workbench.run_debug_action("debug.startConfig", Some(&start), window, cx);
            },
            window,
            cx,
        );
    }

    // ---------------------------------------------------------------------------------------------
    // The Debug tool window

    /// The Debug tool window over the project's debugger, with its console.
    pub(super) fn mount_debug_window(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Panel> {
        let debugger = self.debug_driver()?.read(cx).debugger().clone();
        let root = self.project_root(cx).map(|root| root.to_string_lossy().into_owned());
        let sort = crate::platform::settings::effective(&self.label, cx).debugger.sort_values;
        let view = cx.new(|cx| DebugWindowView::new(debugger.clone(), window, cx));
        let console = cx.new(|cx| DebugConsolePane::new(debugger, cx));
        view.update(cx, |view, cx| view.set_console(Some(console.into()), cx));
        let (frames, variables) = {
            let view = view.read(cx);
            (view.frames().clone(), view.variables().clone())
        };
        frames.update(cx, |frames, cx| frames.set_project_root(root, cx));
        variables.update(cx, |variables, _| variables.set_sort_values(sort));
        let subscriptions = vec![
            cx.subscribe_in(&view, window, Self::debug_window_event),
            cx.subscribe_in(&frames, window, Self::debug_frames_event),
            cx.subscribe_in(&variables, window, Self::debug_variables_event),
        ];
        let slot = self.debug.as_mut()?;
        slot.window = Some(DebugWindowSlot { view: view.clone(), frames, variables, _subscriptions: subscriptions });
        Some(Panel::new(view, None))
    }

    fn debug_variables_view(&self) -> Option<Entity<VariablesView>> {
        self.debug.as_ref()?.window.as_ref().map(|open| open.variables.clone())
    }

    /// Runs a debugger action once this update is over: the action may reach for the workbench.
    fn defer_debug_action(id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        window.defer(cx, move |window, cx| {
            if !execute_action_from(ActionSource::Toolbar, id, None, window, cx) {
                tracing::info!("{id} is not available now");
            }
        });
    }

    fn debug_window_event(
        &mut self,
        _: &Entity<DebugWindowView>,
        event: &DebugWindowEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(driver) = self.debug_driver().cloned() else { return };
        match event.clone() {
            DebugWindowEvent::SelectSession(id) => {
                driver.update(cx, |driver, cx| driver.change(cx, |debugger, _| debugger.state.select_session(id)));
            }
            DebugWindowEvent::CloseTab(id) => {
                let closed = driver.update(cx, |driver, cx| {
                    driver.change(cx, |debugger, host| {
                        let mut ctx = ctx!(debugger, host);
                        debugger.state.close_session_tab(id, &mut ctx)
                    })
                });
                if let CloseTab::Confirm { name } = closed {
                    self.confirm_close_session(id, &name, window, cx);
                }
            }
            DebugWindowEvent::RunAction(id) => Self::defer_debug_action(id, window, cx),
            DebugWindowEvent::ShowMoreMenu(position) => {
                let items = DebugWindowView::more_menu();
                self.show_menu(&items, position, |_, (), _, _| {}, window, cx);
            }
        }
    }

    /// Asks whether to terminate the live session of a tab being closed; yes closes it.
    fn confirm_close_session(
        &mut self,
        id: ide_core::DebugSessionId,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, message, ok, cancel) = {
            let i18n = &mut cx.default_global::<Localization>().0;
            (
                i18n.tx_built(&["Process '", "' Is Running"], &[Param::from(name)]),
                i18n.tx_built(&["Do you want to terminate the process '", "'?"], &[Param::from(name)]),
                i18n.tx("Terminate"),
                i18n.tx("Cancel"),
            )
        };
        let answers = [ok.as_str(), cancel.as_str()];
        let answer = window.prompt(gpui_kit::PromptLevel::Warning, &title, Some(message.as_str()), &answers, cx);
        cx.spawn_in(window, async move |workbench, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = workbench.update(cx, |workbench, cx| {
                let Some(driver) = workbench.debug_driver().cloned() else { return };
                driver.update(cx, |driver, cx| {
                    driver.change(cx, |debugger, host| {
                        let mut ctx = ctx!(debugger, host);
                        debugger.state.close_confirmed(id, &mut ctx);
                    });
                });
            });
        })
        .detach();
    }

    fn debug_frames_event(
        &mut self,
        frames: &Entity<FramesView>,
        event: &FramesViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(driver) = self.debug_driver().cloned() else { return };
        match event.clone() {
            FramesViewEvent::SelectThread { session, thread } => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| debugger.state.select_thread(session, thread));
            }),
            FramesViewEvent::SelectFrame { session, frame, reveal } => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| debugger.state.select_frame(session, frame, reveal));
            }),
            FramesViewEvent::JumpToSource { session, frame } => {
                let opened = driver.read(cx).debugger().read(cx).state.session(session).and_then(|session| {
                    let frame = session.frames.iter().find(|candidate| candidate.id == frame)?;
                    DebugState::reveal_frame(frame, true)
                });
                if let Some(Effect::OpenFile { path, line, column, focus }) = opened {
                    self.open_debug_file(path, line, column, focus, window, cx);
                }
            }
            FramesViewEvent::RunAction(id) => Self::defer_debug_action(id, window, cx),
            FramesViewEvent::ShowFrameMenu(position) => {
                let items = frames.read(cx).frame_menu(cx);
                let frames = frames.clone();
                let run = move |_: &mut Self, command, _: &mut Window, cx: &mut Context<Self>| {
                    frames.update(cx, |frames, cx| frames.run_frame_command(command, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
            FramesViewEvent::ShowThreadMenu(position) => {
                let items = frames.read(cx).thread_menu(cx);
                let frames = frames.clone();
                let run = move |_: &mut Self, thread, _: &mut Window, cx: &mut Context<Self>| {
                    frames.update(cx, |frames, cx| frames.choose_thread(thread, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
            FramesViewEvent::Focused(commands) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| {
                    debugger.focused.focus(commands);
                });
            }),
            FramesViewEvent::Blurred(view) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| {
                    debugger.focused.blur(view);
                });
            }),
        }
    }

    fn debug_variables_event(
        &mut self,
        variables: &Entity<VariablesView>,
        event: &VariablesViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(driver) = self.debug_driver().cloned() else { return };
        let target = variables.downgrade();
        match event.clone() {
            VariablesViewEvent::LoadScopes { request, session, frame } => {
                let loading = driver.read(cx).scopes(session, frame, cx);
                cx.spawn(async move |_, cx| {
                    let scopes = loading.await;
                    let _ = target.update(cx, |variables, cx| variables.scopes_loaded(request, scopes, cx));
                })
                .detach();
            }
            VariablesViewEvent::LoadChildren { request, session, reference } => {
                let loading = driver.read(cx).children(session, reference, cx);
                cx.spawn(async move |_, cx| {
                    let children = loading.await;
                    let _ = target.update(cx, |variables, cx| variables.children_loaded(request, children, cx));
                })
                .detach();
            }
            VariablesViewEvent::Evaluate { request, session, expression, context } => {
                let evaluating = driver.read(cx).evaluate(session, &expression, context, cx);
                cx.spawn(async move |_, cx| {
                    let value = evaluating.await;
                    let _ = target.update(cx, |variables, cx| variables.evaluated(request, value, cx));
                })
                .detach();
            }
            VariablesViewEvent::SetValue { request, session, container, name, evaluate_name, value } => {
                driver.update(cx, |driver, cx| {
                    driver.set_value(request, session, container, &name, evaluate_name.as_deref(), &value, cx);
                });
            }
            VariablesViewEvent::AddWatch(expression) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| debugger.state.add_watch(&expression));
            }),
            VariablesViewEvent::RemoveWatch(expression) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| debugger.state.remove_watch(&expression));
            }),
            // The edited watch moves to the end of the list.
            VariablesViewEvent::EditWatch { old, new } => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| {
                    debugger.state.remove_watch(&old);
                    debugger.state.add_watch(&new);
                });
            }),
            VariablesViewEvent::CopyValue { session, value, reference } => {
                let copying = driver.read(cx).copy_value(session, value, reference, cx);
                cx.spawn(async move |workbench, cx| {
                    let copied = copying.await;
                    let _ = workbench.update(cx, |workbench, cx| match copied {
                        Ok(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                        Err(error) => {
                            let mut request = run_notification(NotificationLevel::Error, "Cannot copy the value");
                            request.body = Some(error);
                            workbench.notify(request, cx);
                        }
                    });
                })
                .detach();
            }
            VariablesViewEvent::AddFieldWatchpoint { session, container, name, label } => {
                let frame = driver.read(cx).debugger().read(cx).state.session(session).and_then(|s| s.selected_frame);
                let asking = driver.read(cx).data_breakpoint_info(session, container, name.clone(), frame, cx);
                cx.spawn(async move |workbench, cx| {
                    let info = asking.await;
                    let _ = workbench.update(cx, |workbench, cx| {
                        let data_id = info.ok().and_then(|info| info.data_id);
                        match (data_id, workbench.debug_driver().cloned()) {
                            (Some(data_id), Some(driver)) => driver.update(cx, |driver, cx| {
                                driver.change(cx, |debugger, host| {
                                    let special = &mut debugger.breakpoints.special;
                                    special.add_field_watchpoint(&data_id, &label, None, None, host.env.now_ms);
                                });
                            }),
                            _ => {
                                let title = format!("“{name}” can't be watched");
                                workbench.notify(run_notification(NotificationLevel::Info, title), cx);
                            }
                        }
                    });
                })
                .detach();
            }
            // The selected frame's source; the declaration of a local is not looked for.
            VariablesViewEvent::JumpToSource { .. } => {
                let opened = driver.read(cx).debugger().read(cx).state.active_session().and_then(|session| {
                    let frame = session.frames.iter().find(|frame| Some(frame.id) == session.selected_frame)?;
                    DebugState::reveal_frame(frame, true)
                });
                if let Some(Effect::OpenFile { path, line, column, focus }) = opened {
                    self.open_debug_file(path, line, column, focus, window, cx);
                }
            }
            VariablesViewEvent::ShowMenu(position) => {
                let items = variables.read(cx).variable_menu(cx);
                let variables = variables.clone();
                let run = move |_: &mut Self, command, window: &mut Window, cx: &mut Context<Self>| {
                    variables.update(cx, |variables, cx| variables.run_variable_command(command, window, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
            VariablesViewEvent::Focused(commands) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| {
                    debugger.focused.focus(commands);
                });
            }),
            VariablesViewEvent::Blurred(view) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, _| {
                    debugger.focused.blur(view);
                });
            }),
        }
    }

    // ---------------------------------------------------------------------------------------------
    // The editors

    /// Listens to `editor`'s gutter, and draws the breakpoints and the execution line in it.
    pub(super) fn watch_debug_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.debug.as_mut() else { return };
        let pressed = cx.subscribe_in(editor, window, |workbench, editor, event: &EditorEvent, _, cx| {
            if let &EditorEvent::GutterPressed { line, shift, alt } = event {
                workbench.debug_gutter_pressed(editor, line, shift, alt, cx);
            }
        });
        slot.gutters.insert(editor.entity_id(), pressed);
        self.sync_debug_marks_of(editor, cx);
    }

    /// A press on `line` of an editor's gutter: a breakpoint goes there, or goes away.
    fn debug_gutter_pressed(
        &mut self,
        editor: &Entity<Editor>,
        line: u32,
        shift: bool,
        alt: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(driver) = self.debug_driver().cloned() else { return };
        let snapshot = EditorSnapshot::of(editor, cx);
        let press = GutterPress { button: GutterButton::Primary, shift, alt };
        let gutter = snapshot.gutter();
        let pressed = driver.update(cx, |driver, cx| driver.gutter_press(&gutter, line, press, cx));
        match pressed {
            PressOutcome::Blocked(reason) => {
                let text = cx.default_global::<Localization>().0.tx(reason);
                let mut request = run_notification(NotificationLevel::Info, text);
                request.key = Some("debug.status".to_owned());
                self.notify(request, cx);
            }
            // The editor reports a click: the release is not told apart from the press.
            PressOutcome::AwaitRelease => {
                let clicked = driver.update(cx, |driver, cx| driver.gutter_click(&gutter, line, press, cx));
                if let ClickOutcome::OpenProperties { id } = clicked {
                    driver.update(cx, |driver, cx| {
                        driver.change(cx, |debugger, _| {
                            let select = crate::debugger::overlays::BreakpointSelection { path: snapshot.path, id };
                            debugger.overlays.open_breakpoints_dialog(Some(select));
                        });
                    });
                }
            }
            PressOutcome::Ignored | PressOutcome::Toggled => {}
        }
    }

    /// Draws the breakpoints and the execution line in every editor of the project, and forgets the
    /// gutters of the editors that are gone.
    fn sync_debug_marks(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let editors = self.editors();
        if let Some(slot) = self.debug.as_mut() {
            slot.gutters.retain(|id, _| editors.iter().any(|editor| editor.entity_id() == *id));
        }
        for editor in &editors {
            self.sync_debug_marks_of(editor, cx);
        }
    }

    fn sync_debug_marks_of(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(driver) = self.debug_driver() else { return };
        let (path, rope) = {
            let editor = editor.read(cx);
            (editor.path().to_string_lossy().into_owned(), editor.rope().clone())
        };
        let (breakpoints, execution) = {
            let debugger = driver.read(cx).debugger().read(cx);
            let lines = buffer::line_count(&rope);
            let running = debugger.state.sessions_running();
            let (stored, special) = (&debugger.breakpoints.lines, &debugger.breakpoints.special);
            let markers = build_markers(stored, special, &path, lines, running);
            let line_mark = |line: u32, kind: MarkKind| Mark::new(buffer::line_range(&rope, line), kind);
            let breakpoints: Vec<Mark> = markers
                .iter()
                .map(|marker| {
                    let kind = match marker.look {
                        BreakpointLook::Unverified => MarkKind::UnverifiedBreakpoint,
                        look if look.tints_line() => MarkKind::BreakpointLine,
                        _ => MarkKind::InactiveBreakpoint,
                    };
                    line_mark(marker.line, kind)
                })
                .collect();
            let point = debugger.state.execution_point();
            let execution: Vec<Mark> = point
                .path
                .as_deref()
                .filter(|at| *at == path && point.line < lines)
                .map(|_| line_mark(point.line, MarkKind::ExecutionLine))
                .into_iter()
                .collect();
            (breakpoints, execution)
        };
        editor.update(cx, |editor, cx| {
            editor.set_marks(MarkLayer::Breakpoints, breakpoints, cx);
            editor.set_marks(MarkLayer::Execution, execution, cx);
        });
    }

    // ---------------------------------------------------------------------------------------------
    // The Breakpoints dialog

    /// Opens the dialog when the debugger asks for it, and closes it when the debugger no longer does.
    fn sync_breakpoints_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.debug.as_ref() else { return };
        let debugger = slot.driver.read(cx).debugger().clone();
        let wanted = debugger.read(cx).overlays.breakpoints.clone();
        match (wanted, slot.breakpoints.is_some()) {
            (Some(select), false) => {
                let dialog = cx.new(|cx| BreakpointsDialog::new(debugger, select, cx));
                let events = cx.subscribe_in(&dialog, window, Self::breakpoints_dialog_event);
                let restore = window.focused(cx);
                window.focus(&dialog.focus_handle(cx), cx);
                dialog.update(cx, |dialog, cx| dialog.opened(cx));
                if let Some(slot) = self.debug.as_mut() {
                    slot.breakpoints = Some(BreakpointsWindow { dialog, restore, _events: events });
                }
                cx.notify();
            }
            (None, true) => {
                let open = self.debug.as_mut().and_then(|slot| slot.breakpoints.take());
                let restore = open.and_then(|open| open.restore);
                if let Some(target) = restore.or_else(|| self.editor().map(|editor| editor.focus_handle(cx))) {
                    window.focus(&target, cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn breakpoints_dialog_event(
        &mut self,
        dialog: &Entity<BreakpointsDialog>,
        event: &BreakpointsDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(driver) = self.debug_driver().cloned() else { return };
        let close = |debugger: &mut crate::debugger::actions::Debugger| debugger.overlays.breakpoints = None;
        match event.clone() {
            BreakpointsDialogEvent::UpdateLine { path, id, patch } => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, host| {
                    debugger.breakpoints.lines.update_breakpoint(&path, &id, host.env.now_ms, |b| patch.apply(b));
                });
            }),
            BreakpointsDialogEvent::UpdateSpecial { id, patch } => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, host| {
                    debugger.breakpoints.special.update_special(&id, patch, host.env.now_ms);
                });
            }),
            BreakpointsDialogEvent::RemoveLine { path, id } => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, host| {
                    debugger.breakpoints.lines.remove_breakpoint(&path, &id, host.env.now_ms);
                });
            }),
            BreakpointsDialogEvent::RemoveSpecial(id) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, host| debugger.breakpoints.special.remove_special(&id, host.env.now_ms));
            }),
            BreakpointsDialogEvent::RemoveAll => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, host| {
                    debugger.breakpoints.lines.remove_all_breakpoints(host.env.now_ms);
                    debugger.breakpoints.special.remove_all_special(host.env.now_ms);
                });
            }),
            BreakpointsDialogEvent::AddMethodBreakpoint => {
                let added = driver.update(cx, |driver, cx| {
                    driver.change(cx, |debugger, host| {
                        debugger.breakpoints.special.add_method_breakpoint("", None, host.env.now_ms).id
                    })
                });
                dialog.update(cx, |dialog, cx| dialog.select(&added, cx));
            }
            BreakpointsDialogEvent::SetMuted(muted) => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, host| debugger.breakpoints.set_muted(muted, host.env.now_ms));
            }),
            BreakpointsDialogEvent::SetExceptionFilter { adapter, filter, enabled } => {
                driver.update(cx, |driver, cx| {
                    driver.change(cx, |debugger, host| {
                        let can_sync = host.env.can_sync();
                        debugger.breakpoints.exceptions.set_exception_filter(&adapter, &filter, enabled, can_sync);
                    });
                })
            }
            BreakpointsDialogEvent::RefreshExceptionFilters => driver.update(cx, |driver, cx| {
                driver.change(cx, |debugger, host| debugger.breakpoints.exceptions.refresh(host.env.desktop));
            }),
            BreakpointsDialogEvent::GoToSource { path, line } => {
                driver.update(cx, |driver, cx| driver.change(cx, |debugger, _| close(debugger)));
                self.open_debug_file(path, line, 0, true, window, cx);
            }
            BreakpointsDialogEvent::Close => {
                driver.update(cx, |driver, cx| driver.change(cx, |debugger, _| close(debugger)));
            }
        }
    }

    /// The Breakpoints dialog over the window, while it is open.
    pub(super) fn debug_breakpoints_layer(&self) -> Option<AnyElement> {
        let dialog = self.breakpoints_dialog()?.clone();
        let layer = div()
            .id("debug-breakpoints-layer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .child(dialog);
        Some(gpui_kit::deferred(layer).with_priority(1).into_any_element())
    }
}

/// The debug console of the shown session: its output, read-only.
pub struct DebugConsolePane {
    debugger: Entity<crate::debugger::actions::Debugger>,
    scroll: UniformListScrollHandle,
    _observer: Subscription,
}

impl DebugConsolePane {
    fn new(debugger: Entity<crate::debugger::actions::Debugger>, cx: &mut Context<Self>) -> Self {
        let observer = cx.observe(&debugger, |_, _, cx| cx.notify());
        Self { debugger, scroll: UniformListScrollHandle::new(), _observer: observer }
    }
}

impl Render for DebugConsolePane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = {
            let state = &self.debugger.read(cx).state;
            state.active().map_or(0, |session| state.console(session).len())
        };
        let family = cx
            .try_global::<theme::ActiveTheme>()
            .and_then(|active| active.theme.font_families("font-mono").into_iter().next())
            .unwrap_or_else(|| theme::fonts::BUNDLED_MONO_FAMILY.to_owned());
        let list = uniform_list(
            "debug-console-lines",
            count,
            cx.processor(|pane: &mut Self, range: std::ops::Range<usize>, _, cx| {
                let state = &pane.debugger.read(cx).state;
                let texts: Vec<String> = state.active().map_or_else(Vec::new, |session| {
                    let lines = state.console(session).get(range.clone()).unwrap_or_default();
                    lines.iter().map(|line| line.text.trim_end_matches('\n').to_owned()).collect()
                });
                texts
                    .into_iter()
                    .map(|text| div().h(px(18.0)).px(px(8.0)).whitespace_nowrap().child(SharedString::from(text)))
                    .collect()
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();
        div()
            .size_full()
            .font_family(SharedString::from(family))
            .text_size(theme::length(cx, "fs-small"))
            .bg(theme::color(cx, "bg-editor"))
            .text_color(theme::color(cx, "fg-editor"))
            .child(list)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::backend::{AppDirs, Backend};
    use crate::debugger::breakpoints::store::storage_key;
    use crate::debugger::breakpoints_view::DialogRow;
    use crate::debugger::state::SessionStatus;
    use crate::debugger::variables_view::VariableCommand;
    use crate::editor::view::FOLD_GUTTER;
    use crate::platform::actions::execute_action_or_throw;
    use crate::platform::context::window_context;
    use crate::platform::input;
    use crate::platform::storage::UiStorage;
    use crate::run::configs::blank_config;
    use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, TestAppContext, VisualTestContext, point};
    use std::time::Duration;
    mod proofs3_debug;
    mod proofs6_debug;
    mod proofs8_recent;
    mod proofs_debug;

    /// A debug adapter speaking DAP over stdio. It runs the launched `program` file line by line: it
    /// stops at a line with a breakpoint, steps one line on `next`, `stepIn` and `stepOut`, runs to
    /// the next breakpoint on `continue` and ends the program after its last line. A line that reads `loop` keeps the
    /// program running until it is stopped. The only frame is `main` at the current line, with the
    /// locals `line`, holding the 1-based line (or the value it was set to), and `point`, a structure
    /// with `x = 1`. An expression evaluates to `value of <expression>`. It exits when its input
    /// closes.
    /// A stop is reported 0.2 s after the request that led to it, as a program takes time to run:
    /// the backend reports a step or a resume as continued once its answer is in, and a stop
    /// reported before that would be overwritten.
    pub(in crate::workbench) const FAKE_ADAPTER: &str = r#"import json, os, sys, time
out = sys.stdout.buffer
seq = 0
def send(message):
    global seq
    seq += 1
    message['seq'] = seq
    body = json.dumps(message).encode()
    out.write(b'Content-Length: %d\r\n\r\n' % len(body) + body)
    out.flush()
def respond(request, body=None):
    send({'type': 'response', 'request_seq': request['seq'], 'command': request['command'],
          'success': True, 'body': body or {}})
def event(name, body=None):
    send({'type': 'event', 'event': name, 'body': body or {}})
def read():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            if length is not None:
                return json.loads(sys.stdin.buffer.read(length))
            continue
        key, _, value = line.decode().partition(':')
        if key.strip().lower() == 'content-length':
            length = int(value.strip())
source, lines, breakpoints, current, launch, ended, values = None, [], set(), 0, None, False, {}
def stop(reason):
    time.sleep(0.2)
    event('stopped', {'reason': reason, 'threadId': 1, 'allThreadsStopped': True})
def finish():
    global ended, current
    if not ended:
        ended = True
        current = 0
        event('exited', {'exitCode': 0})
        event('terminated')
def run(start):
    global current
    for number in range(start, len(lines) + 1):
        current = number
        if number in breakpoints:
            stop('breakpoint')
            return
        if lines[number - 1].strip() == 'loop':
            return
    finish()
while True:
    request = read()
    if request is None:
        sys.exit(0)
    if request.get('type') != 'request':
        continue
    command = request['command']
    arguments = request.get('arguments') or {}
    if command == 'initialize':
        respond(request, {'supportsConfigurationDoneRequest': True, 'supportsSetVariable': True})
    elif command == 'launch':
        launch = request
        source = os.path.join(arguments.get('cwd') or '.', arguments['program'])
        with open(source) as program:
            lines = program.read().splitlines()
        event('initialized')
    elif command == 'setBreakpoints':
        wanted = [b['line'] for b in arguments.get('breakpoints', [])]
        breakpoints = set(wanted)
        respond(request, {'breakpoints': [{'id': i + 1, 'verified': True, 'line': l} for i, l in enumerate(wanted)]})
    elif command == 'configurationDone':
        respond(request)
        if launch is not None:
            respond(launch)
            launch = None
        run(1)
    elif command == 'threads':
        respond(request, {'threads': [{'id': 1, 'name': 'main'}]})
    elif command == 'stackTrace':
        frames = []
        if current and not ended:
            frames = [{'id': 1000 + current, 'name': 'main', 'line': current, 'column': 1,
                       'source': {'name': os.path.basename(source), 'path': source}}]
        respond(request, {'stackFrames': frames, 'totalFrames': len(frames)})
    elif command == 'scopes':
        respond(request, {'scopes': [{'name': 'Locals', 'variablesReference': 1, 'expensive': False}]})
    elif command == 'variables':
        if arguments.get('variablesReference') == 2:
            found = [{'name': 'x', 'value': '1', 'type': 'int', 'variablesReference': 0}]
        else:
            found = [{'name': 'line', 'value': values.get('line', str(current)), 'type': 'int',
                      'variablesReference': 0},
                     {'name': 'point', 'value': 'Point', 'type': 'Point', 'variablesReference': 2}]
        respond(request, {'variables': found})
    elif command == 'setVariable':
        values[arguments['name']] = arguments['value']
        respond(request, {'value': arguments['value'], 'type': 'int'})
    elif command == 'evaluate':
        respond(request, {'result': 'value of ' + arguments['expression'], 'variablesReference': 0})
    elif command == 'continue':
        respond(request, {'allThreadsContinued': True})
        run(current + 1)
    elif command in ('next', 'stepIn', 'stepOut'):
        respond(request)
        if current < len(lines):
            current += 1
            stop('step')
        else:
            finish()
    elif command == 'pause':
        respond(request)
        stop('pause')
    elif command == 'disconnect':
        respond(request)
        finish()
        sys.exit(0)
    else:
        respond(request)
"#;

    /// A three-line program: the breakpoint goes on `middle`.
    const PROGRAM: &str = "start\nmiddle\nend\n";

    struct Fixture {
        workbench: Entity<Workbench>,
        root: PathBuf,
        _folders: (tempfile::TempDir, tempfile::TempDir),
    }

    impl Fixture {
        fn editor(&self, cx: &mut VisualTestContext) -> Entity<Editor> {
            self.workbench.read_with(cx, |workbench, _| workbench.editor().cloned()).expect("an editor")
        }

        fn driver(&self, cx: &mut VisualTestContext) -> Entity<DebugDriver> {
            self.workbench.read_with(cx, |workbench, _| workbench.debug_driver().cloned()).expect("a debugger")
        }

        fn view(&self, cx: &mut VisualTestContext) -> Option<Entity<DebugWindowView>> {
            self.workbench.read_with(cx, |workbench, _| workbench.debug_window_view().cloned())
        }

        fn marks(&self, layer: MarkLayer, cx: &mut VisualTestContext) -> Vec<Mark> {
            self.editor(cx).read_with(cx, |editor, _| editor.marks(layer).to_vec())
        }

        /// The mark of `kind` over the 0-based `line` of the editor's document.
        fn line_mark(&self, line: u32, kind: MarkKind, cx: &mut VisualTestContext) -> Mark {
            self.editor(cx).read_with(cx, |editor, _| Mark::new(buffer::line_range(editor.rope(), line), kind))
        }

        fn caret_to(&self, line: u32, cx: &mut VisualTestContext) {
            self.editor(cx).update(cx, |editor, cx| {
                let at = editor.offset_of(line, 0);
                editor.select(at, at, cx);
            });
        }

        fn status(&self, cx: &mut VisualTestContext) -> Option<SessionStatus> {
            let driver = self.driver(cx);
            driver.read_with(cx, |driver, cx| driver.debugger().read(cx).state.active_session().map(|s| s.status))
        }

        fn console(&self, cx: &mut VisualTestContext) -> String {
            let driver = self.driver(cx);
            driver.read_with(cx, |driver, cx| {
                let state = &driver.debugger().read(cx).state;
                let lines = state.active().map(|id| state.console(id)).unwrap_or_default();
                lines.iter().map(|line| line.text.as_str()).collect()
            })
        }

        fn frame_lines(&self, cx: &mut VisualTestContext) -> Vec<String> {
            let Some(view) = self.view(cx) else { return Vec::new() };
            let frames = view.read_with(cx, |view, _| view.frames().clone());
            frames.update(cx, |frames, cx| frames.frame_lines(cx))
        }

        fn variable_lines(&self, cx: &mut VisualTestContext) -> Vec<String> {
            let Some(view) = self.view(cx) else { return Vec::new() };
            let variables = view.read_with(cx, |view, _| view.variables().clone());
            variables.read_with(cx, |variables, cx| variables.variable_lines(cx))
        }

        /// Whether the local `line` is listed with `value`.
        fn shows_line_value(&self, value: &str, cx: &mut VisualTestContext) -> bool {
            self.variable_lines(cx).iter().any(|row| row.trim_start().starts_with("line") && row.ends_with(value))
        }

        /// Selects the project's configuration once the run driver has listed it.
        fn select_main(&self, cx: &mut VisualTestContext) {
            let run = self.workbench.read_with(cx, |workbench, _| workbench.run_driver().cloned()).expect("runs");
            let store = run.read_with(cx, |run, _| run.store().clone());
            long(cx, "listing the configurations", &|cx| store.read_with(cx, |store, _| store.loaded));
            run.update(cx, |run, cx| run.select_config(Some("main"), cx));
        }
    }

    /// A trusted project with `program` in `main.fake`, debugged by the fake adapter through the
    /// configuration `main`, open in a window with `main.fake` in the editor.
    fn open<'a>(cx: &'a mut TestAppContext, program: &str) -> (Fixture, &'a mut VisualTestContext) {
        cx.executor().allow_parking();
        let (home, folder) = (tempfile::tempdir().expect("a home"), tempfile::tempdir().expect("a folder"));
        let dirs = AppDirs::under(home.path());
        let script = home.path().join("fake_dap.py");
        std::fs::write(&script, FAKE_ADAPTER).expect("the adapter script is written");
        let languages = dirs.config.join("languages");
        std::fs::create_dir_all(&languages).expect("the languages folder is made");
        let toml = format!(
            "id = \"fakedap\"\nname = \"Fake\"\nextensions = [\"fake\"]\nline-comment = \"#\"\n\n\
             [debugger]\nadapter = \"fake-dap\"\ncommand = \"python3\"\nargs = [{:?}]\n",
            script.to_string_lossy(),
        );
        std::fs::write(languages.join("fakedap.toml"), toml).expect("the language is written");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("main.fake"), program).expect("the program is written");
        let config = ide_run::RunConfig {
            command: "main.fake".to_owned(),
            debug_adapter: Some("fake-dap".to_owned()),
            ..blank_config("main", "main", "program")
        };
        ide_run::save_user_configs(&root, &[config]).expect("the configuration is saved");
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            input::init(cx);
            crate::editor::actions::register(cx);
            super::super::register_actions(cx);
            cx.set_global(UiStorage::default());
            Backend::init(dirs, cx).expect("the backend starts");
            super::super::menu::init(cx);
            cx.global::<Backend>().host.trust.decide(&root, true, false).expect("the project is trusted");
        });
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        long(cx, "the project to open", &|cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let file = root.join("main.fake");
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project_file(file, window, cx));
        long(cx, "the editor", &|cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        (Fixture { workbench, root, _folders: (home, folder) }, cx)
    }

    /// Lets the backend, the adapter and the timers run, real time passing, until `done` holds.
    fn long(cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool) {
        for _ in 0..3000 {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{what} did not happen within thirty seconds");
    }

    fn run(cx: &mut VisualTestContext, id: &str) {
        let ran = cx.update(|window, cx| execute_action_or_throw(id, None, window, cx));
        ran.unwrap_or_else(|error| panic!("{id}: {error}"));
    }

    fn context(cx: &mut VisualTestContext, key: &str) -> bool {
        cx.update(|window, cx| window_context(window, cx).is_truthy(key))
    }

    fn close_project(fixture: &Fixture, cx: &mut VisualTestContext) {
        fixture.workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
        cx.run_until_parked();
    }

    // Not from a spec: Toggle Line Breakpoint at the caret marks the line in the editor, stores the
    // breakpoint for the project, lists it in View Breakpoints, and Mute Breakpoints greys it.
    #[gpui_kit::test]
    async fn toggling_a_breakpoint_at_a_line_marks_it_persists_it_and_lists_it_in_the_breakpoints_dialog(
        cx: &mut TestAppContext,
    ) {
        let (fixture, cx) = open(cx, PROGRAM);
        fixture.caret_to(1, cx);
        run(cx, "debug.toggleBreakpoint");
        let marked = fixture.line_mark(1, MarkKind::BreakpointLine, cx);
        long(cx, "the breakpoint mark", &|cx| fixture.marks(MarkLayer::Breakpoints, cx) == [marked.clone()]);
        let key = storage_key(&fixture.root.to_string_lossy());
        let stored = cx.update(|_, cx| cx.global::<UiStorage>().get(&key).map(str::to_owned));
        assert!(stored.as_deref().is_some_and(|value| value.contains("\"line\":1")), "{stored:?}");

        run(cx, "debug.viewBreakpoints");
        let dialog_open =
            |cx: &mut VisualTestContext| fixture.workbench.read_with(cx, |w, _| w.breakpoints_dialog().is_some());
        long(cx, "the Breakpoints dialog", &dialog_open);
        let dialog = fixture.workbench.read_with(cx, |w, _| w.breakpoints_dialog().cloned()).expect("the dialog");
        let listed = dialog.read_with(cx, |dialog, _| {
            dialog.rows().iter().any(|row| matches!(row, DialogRow::Line { entry, .. } if entry.line == 1))
        });
        assert!(listed);

        run(cx, "debug.muteBreakpoints");
        let muted = fixture.line_mark(1, MarkKind::InactiveBreakpoint, cx);
        long(cx, "the muted mark", &|cx| fixture.marks(MarkLayer::Breakpoints, cx) == [muted.clone()]);
        assert!(dialog.read_with(cx, |dialog, _| dialog.muted()));
        dialog.update(cx, |_, cx| cx.emit(BreakpointsDialogEvent::Close));
        long(cx, "the dialog to close", &|cx| !dialog_open(cx));
        close_project(&fixture, cx);
    }

    // Not from a spec: a click in the gutter toggles the line's breakpoint.
    #[gpui_kit::test]
    async fn clicking_the_gutter_toggles_the_lines_breakpoint(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, PROGRAM);
        let editor = fixture.editor(cx);
        let at = editor.read_with(cx, |editor, _| editor.position_of(editor.offset_of(2, 0)));
        let at = at.expect("line 3 is painted");
        let press = |cx: &mut VisualTestContext| {
            // Left of the fold chevrons' column, where the breakpoints are.
            let position = point(at.x - gpui_kit::px(FOLD_GUTTER + 12.0), at.y + gpui_kit::px(4.0));
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            });
        };
        press(cx);
        let marked = fixture.line_mark(2, MarkKind::BreakpointLine, cx);
        long(cx, "the breakpoint mark", &|cx| fixture.marks(MarkLayer::Breakpoints, cx) == [marked.clone()]);
        press(cx);
        long(cx, "the mark to go", &|cx| fixture.marks(MarkLayer::Breakpoints, cx).is_empty());
        close_project(&fixture, cx);
    }

    // Not from a spec: Run › Debug starts the selected configuration under the debugger. The Debug
    // tool window shows its tab, the program stops at the breakpoint with its frame listed, the
    // execution line drawn and the local shown, Step Over moves the line, and Resume runs the
    // program to its end, which the tab shows.
    #[gpui_kit::test]
    async fn debugging_a_configuration_stops_at_the_breakpoint_steps_over_and_resumes_to_the_end(
        cx: &mut TestAppContext,
    ) {
        let (fixture, cx) = open(cx, PROGRAM);
        fixture.caret_to(1, cx);
        run(cx, "debug.toggleBreakpoint");
        fixture.select_main(cx);
        run(cx, "run.debug");
        long(cx, "the stop at the breakpoint", &|cx| context(cx, "debugSessionPaused"));
        assert!(fixture.workbench.read_with(cx, |w, _| w.is_tool_window_visible(DEBUG_TOOL_WINDOW)));
        let view = fixture.view(cx).expect("the Debug tool window");
        assert_eq!(view.update(cx, |view, cx| view.tab_titles(cx)), ["main"]);
        long(cx, "the frame", &|cx| fixture.frame_lines(cx) == ["main, main.fake:2"]);
        let execution = fixture.line_mark(1, MarkKind::ExecutionLine, cx);
        long(cx, "the execution line", &|cx| fixture.marks(MarkLayer::Execution, cx) == [execution.clone()]);
        long(cx, "the local", &|cx| fixture.shows_line_value("2", cx));

        run(cx, "debug.stepOver");
        let stepped = fixture.line_mark(2, MarkKind::ExecutionLine, cx);
        long(cx, "the step", &|cx| fixture.marks(MarkLayer::Execution, cx) == [stepped.clone()]);
        long(cx, "the stepped frame", &|cx| fixture.frame_lines(cx) == ["main, main.fake:3"]);
        long(cx, "the local after the step", &|cx| fixture.shows_line_value("3", cx));

        run(cx, "debug.resume");
        long(cx, "the end of the program", &|cx| fixture.status(cx) == Some(SessionStatus::Terminated));
        assert_eq!(view.read_with(cx, |view, cx| view.finished_tabs(cx)), [true]);
        assert!(fixture.console(cx).contains("Process finished with exit code 0"), "{}", fixture.console(cx));
        assert!(fixture.marks(MarkLayer::Execution, cx).is_empty());
        assert!(!context(cx, "debugSessionActive"));
        close_project(&fixture, cx);
    }

    // Not from a spec: Stop ends a session whose program is running.
    #[gpui_kit::test]
    async fn stop_ends_a_running_debug_session(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, "start\nloop\nend\n");
        fixture.select_main(cx);
        run(cx, "run.debug");
        long(cx, "the running session", &|cx| context(cx, "debugSessionRunning"));
        run(cx, "debug.stop");
        long(cx, "the end of the session", &|cx| fixture.status(cx) == Some(SessionStatus::Terminated));
        let view = fixture.view(cx).expect("the Debug tool window");
        assert_eq!(view.read_with(cx, |view, cx| view.finished_tabs(cx)), [true]);
        assert!(!context(cx, "debugSessionActive"));
        close_project(&fixture, cx);
    }

    // Not from a spec: in Restricted Mode Run › Debug starts no session; once the project is trusted
    // the same configuration runs under the debugger.
    #[gpui_kit::test]
    async fn a_project_in_restricted_mode_debugs_nothing_until_it_is_trusted(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, "start\nloop\nend\n");
        let restricted = |cx: &mut VisualTestContext| fixture.workbench.read_with(cx, |w, cx| w.project_restricted(cx));
        run(cx, "project.revokeTrust");
        long(cx, "Restricted Mode", &restricted);
        fixture.select_main(cx);
        run(cx, "run.debug");
        for _ in 0..50 {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(session_count(&fixture, cx), 0, "no session starts");
        assert!(!context(cx, "debugSessionActive"));

        run(cx, "project.trust");
        long(cx, "the trust", &|cx| !restricted(cx));
        fixture.select_main(cx);
        run(cx, "run.debug");
        long(cx, "the running session", &|cx| context(cx, "debugSessionRunning"));
        run(cx, "debug.stop");
        long(cx, "the end of the session", &|cx| fixture.status(cx) == Some(SessionStatus::Terminated));
        close_project(&fixture, cx);
    }

    fn session_count(fixture: &Fixture, cx: &mut VisualTestContext) -> usize {
        let driver = fixture.driver(cx);
        driver.read_with(cx, |driver, cx| driver.debugger().read(cx).state.sessions().len())
    }

    fn focus<V: Focusable>(cx: &mut VisualTestContext, view: &Entity<V>) {
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.run_until_parked();
    }

    // Not from a spec: the debugger's keys act where their `when` / `keyWhen` let them. Without a
    // session the stepping and resuming keys do nothing in an editor; ⌘⌥⇧F8, ⌘⇧F8, ⌘5, ⌘F8 and ⌃D
    // toggle a temporary breakpoint, open the Breakpoints dialog, show the Debug tool window, toggle
    // the caret line's breakpoint and debug the selected configuration; while paused F4 and F2 in
    // the editor are neither Jump to Source nor Set Value, F2 in the Variables view is, F8 steps and
    // F9 resumes from the editor; ⌘R reruns the session only from the Debug tool window.
    #[gpui_kit::test]
    async fn the_debuggers_keys_act_only_where_their_conditions_hold(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, "start\nmiddle\nend\nlast\n");
        let editor = fixture.editor(cx);
        focus(cx, &editor);
        let text = editor.read_with(cx, |editor, _| editor.text());
        for key in ["f7", "f8", "shift-f8", "f9", "cmd-alt-r", "alt-f9", "alt-f10", "shift-f7", "alt-shift-f8"] {
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
            assert_eq!(fixture.status(cx), None, "{key}");
        }
        assert_eq!(editor.read_with(cx, |editor, _| editor.text()), text);
        assert!(!fixture.workbench.read_with(cx, |w, _| w.is_tool_window_visible(DEBUG_TOOL_WINDOW)));
        assert!(fixture.marks(MarkLayer::Breakpoints, cx).is_empty());

        // ⌘⌥⇧F8 toggles a temporary breakpoint, ⌘⇧F8 opens the Breakpoints dialog (Escape closes it).
        fixture.caret_to(2, cx);
        cx.simulate_keystrokes("cmd-alt-shift-f8");
        long(cx, "the temporary breakpoint", &|cx| fixture.marks(MarkLayer::Breakpoints, cx).len() == 1);
        cx.simulate_keystrokes("cmd-alt-shift-f8");
        long(cx, "no breakpoint", &|cx| fixture.marks(MarkLayer::Breakpoints, cx).is_empty());
        let dialog_open =
            |cx: &mut VisualTestContext| fixture.workbench.read_with(cx, |w, _| w.breakpoints_dialog().is_some());
        cx.simulate_keystrokes("cmd-shift-f8");
        long(cx, "the Breakpoints dialog", &dialog_open);
        cx.simulate_keystrokes("escape");
        long(cx, "the dialog to close", &|cx| !dialog_open(cx));
        // ⌘5 shows and hides the Debug tool window.
        cx.simulate_keystrokes("cmd-5");
        long(cx, "the Debug tool window", &|cx| {
            fixture.workbench.read_with(cx, |w, _| w.is_tool_window_visible(DEBUG_TOOL_WINDOW))
        });
        cx.simulate_keystrokes("cmd-5");
        long(cx, "the Debug tool window to hide", &|cx| {
            !fixture.workbench.read_with(cx, |w, _| w.is_tool_window_visible(DEBUG_TOOL_WINDOW))
        });
        focus(cx, &editor);

        fixture.caret_to(1, cx);
        cx.simulate_keystrokes("cmd-f8");
        let marked = fixture.line_mark(1, MarkKind::BreakpointLine, cx);
        long(cx, "the breakpoint mark", &|cx| fixture.marks(MarkLayer::Breakpoints, cx) == [marked.clone()]);
        // ⌃D debugs the selected configuration.
        fixture.select_main(cx);
        cx.simulate_keystrokes("ctrl-d");
        long(cx, "the stop at the breakpoint", &|cx| context(cx, "debugSessionPaused"));
        long(cx, "the local", &|cx| fixture.shows_line_value("2", cx));
        let view = fixture.view(cx).expect("the Debug tool window");
        let variables = view.read_with(cx, |view, _| view.variables().clone());

        // Paused, with the keyboard in the editor: F4 does not jump to the frame, F2 sets no value.
        focus(cx, &editor);
        fixture.caret_to(3, cx);
        assert!(!context(cx, "debugVariablesFocus") && !context(cx, "debugFramesFocus"));
        cx.simulate_keystrokes("f4");
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        assert_eq!(editor.read_with(cx, |editor, _| editor.caret_position()), (3, 0));
        assert!(!variables.read_with(cx, |variables, _| variables.is_editing()));
        assert_eq!(fixture.status(cx), Some(SessionStatus::Paused));

        // In the Variables view, F2 edits the selected value.
        focus(cx, &variables);
        assert!(context(cx, "debugVariablesFocus"));
        for _ in 0..4 {
            if variables.read_with(cx, |variables, cx| variables.selected_name(cx)).as_deref() == Some("line") {
                break;
            }
            cx.simulate_keystrokes("down");
            cx.run_until_parked();
        }
        assert_eq!(variables.read_with(cx, |variables, cx| variables.selected_name(cx)).as_deref(), Some("line"));
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        assert!(variables.read_with(cx, |variables, _| variables.is_editing()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // F8 steps and F9 resumes with the keyboard in the editor.
        focus(cx, &editor);
        cx.simulate_keystrokes("f8");
        let stepped = fixture.line_mark(2, MarkKind::ExecutionLine, cx);
        long(cx, "the step", &|cx| fixture.marks(MarkLayer::Execution, cx) == [stepped.clone()]);
        cx.simulate_keystrokes("f9");
        long(cx, "the end of the program", &|cx| fixture.status(cx) == Some(SessionStatus::Terminated));

        // ⌘R reruns the session from the Debug tool window only.
        assert!(context(cx, "debugCanRerun"));
        let first = active_session(&fixture, cx).expect("the finished session is listed");
        focus(cx, &editor);
        cx.simulate_keystrokes("cmd-r");
        // Long enough for a rerun to have started and stopped at its breakpoint.
        for _ in 0..50 {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(active_session(&fixture, cx), Some(first));
        assert_eq!(fixture.status(cx), Some(SessionStatus::Terminated));
        focus(cx, &view);
        cx.simulate_keystrokes("cmd-r");
        long(cx, "the rerun to stop at the breakpoint", &|cx| {
            active_session(&fixture, cx).is_some_and(|session| session != first) && context(cx, "debugSessionPaused")
        });
        close_project(&fixture, cx);
    }

    fn notification_titles(fixture: &Fixture, cx: &mut VisualTestContext) -> Vec<String> {
        fixture.workbench.read_with(cx, |w, _| w.notifications().items().iter().map(|n| n.title.clone()).collect())
    }

    fn active_session(fixture: &Fixture, cx: &mut VisualTestContext) -> Option<ide_core::DebugSessionId> {
        let driver = fixture.driver(cx);
        driver.read_with(cx, |driver, cx| driver.debugger().read(cx).state.active())
    }

    fn caret_line(fixture: &Fixture, cx: &mut VisualTestContext) -> u32 {
        fixture.editor(cx).read_with(cx, |editor, _| editor.caret_position().0)
    }

    fn wait_execution_at(fixture: &Fixture, line: u32, what: &str, cx: &mut VisualTestContext) {
        let mark = fixture.line_mark(line, MarkKind::ExecutionLine, cx);
        long(cx, what, &|cx| {
            context(cx, "debugSessionPaused") && fixture.marks(MarkLayer::Execution, cx) == [mark.clone()]
        });
    }

    // Not from a spec: the rest of the debugger's actions in a window with a project. Temporary
    // breakpoints and Remove All; method and field breakpoints where there is no declaration; Run to
    // Cursor, Step Into, Step Out, Show Execution Point, F4 in the Frames view, Force Step Over; Debug
    // Configuration by id, Pause and Stop; the hot swap and Dart actions, which do nothing here.
    #[gpui_kit::test]
    async fn the_debuggers_other_actions_reach_the_adapter_and_the_editor(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, "a\nb\nc\nd\ne\nloop\n");
        fixture.caret_to(0, cx);
        run(cx, "debug.toggleTemporaryBreakpoint");
        long(cx, "the temporary breakpoint", &|cx| fixture.marks(MarkLayer::Breakpoints, cx).len() == 1);
        run(cx, "debug.removeAllBreakpoints");
        long(cx, "no breakpoint", &|cx| fixture.marks(MarkLayer::Breakpoints, cx).is_empty());
        run(cx, "debug.toggleMethodBreakpoint");
        run(cx, "debug.toggleFieldWatchpoint");
        long(cx, "the notices", &|cx| {
            let titles = notification_titles(&fixture, cx);
            titles.iter().any(|t| t == "No method declaration on this line")
                && titles.iter().any(|t| t == "No field declaration on this line")
        });

        fixture.caret_to(1, cx);
        run(cx, "debug.toggleBreakpoint");
        fixture.select_main(cx);
        run(cx, "run.debug");
        wait_execution_at(&fixture, 1, "the stop at the breakpoint", cx);
        fixture.caret_to(3, cx);
        run(cx, "debug.runToCursor");
        wait_execution_at(&fixture, 3, "the run to the cursor", cx);
        run(cx, "debug.stepInto");
        wait_execution_at(&fixture, 4, "the step into", cx);
        run(cx, "debug.stepOut");
        wait_execution_at(&fixture, 5, "the step out", cx);
        fixture.caret_to(0, cx);
        run(cx, "debug.showExecutionPoint");
        long(cx, "the execution point", &|cx| caret_line(&fixture, cx) == 5);

        // F4 in the Frames view opens the frame's line.
        fixture.caret_to(0, cx);
        let view = fixture.view(cx).expect("the Debug tool window");
        let frames = view.read_with(cx, |view, _| view.frames().clone());
        focus(cx, &frames);
        assert!(context(cx, "debugFramesFocus"));
        cx.simulate_keystrokes("f4");
        long(cx, "the jump to the frame", &|cx| caret_line(&fixture, cx) == 5);

        // The hot swap and Dart actions have nothing to act on.
        for id in ["debug.reloadChangedClasses", "debug.cancelHotSwap", "debug.hotReload", "debug.hotRestart"] {
            fixture.workbench.update_in(cx, |w, window, cx| w.run_debug_action(id, None, window, cx));
            cx.run_until_parked();
            assert_eq!(fixture.status(cx), Some(SessionStatus::Paused), "{id}");
        }
        run(cx, "debug.forceStepOver");
        long(cx, "the end of the program", &|cx| fixture.status(cx) == Some(SessionStatus::Terminated));
        let first = active_session(&fixture, cx).expect("the finished session is listed");

        // Debug Configuration by id: the program stops at its breakpoint, runs into its loop, pauses
        // there and stops.
        let start = cx.update(|window, cx| {
            execute_action_or_throw("run.debugConfig", Some(&serde_json::json!("main")), window, cx)
        });
        start.expect("run.debugConfig runs");
        wait_execution_at(&fixture, 1, "the second stop at the breakpoint", cx);
        // A new session replaces the finished one.
        assert_eq!(session_count(&fixture, cx), 1);
        assert_ne!(active_session(&fixture, cx), Some(first));
        run(cx, "debug.resume");
        long(cx, "the loop", &|cx| context(cx, "debugSessionRunning"));
        run(cx, "debug.pause");
        wait_execution_at(&fixture, 5, "the pause", cx);
        run(cx, "debug.stop");
        long(cx, "the stop", &|cx| fixture.status(cx) == Some(SessionStatus::Terminated));
        run(cx, "debug.showToolWindow");
        assert!(fixture.workbench.read_with(cx, |w, _| w.is_tool_window_visible(DEBUG_TOOL_WINDOW)));
        close_project(&fixture, cx);
    }

    fn select_variable(variables: &Entity<VariablesView>, name: &str, cx: &mut VisualTestContext) {
        focus(cx, variables);
        for _ in 0..12 {
            if variables.read_with(cx, |variables, cx| variables.selected_name(cx)).as_deref() == Some(name) {
                return;
            }
            cx.simulate_keystrokes("down");
            cx.run_until_parked();
        }
        panic!("{name} is not among {:?}", variables.read_with(cx, |variables, cx| variables.variable_lines(cx)));
    }

    // Not from a spec: what the Debug tool window's views ask of the window while the program is
    // paused. The evaluate field shows an expression's value and adds it as a watch, Delete removes
    // the watch, Set Value (F2) writes the local, Copy Value copies a structure with its fields, a
    // breakpoint's Go to Source in the Breakpoints dialog opens its line, and closing the tab of the
    // live session asks first and then terminates it.
    #[gpui_kit::test]
    async fn the_debug_views_evaluate_watch_set_and_copy_values_and_close_a_live_session(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, PROGRAM);
        fixture.caret_to(1, cx);
        run(cx, "debug.toggleBreakpoint");
        fixture.select_main(cx);
        run(cx, "run.debug");
        long(cx, "the stop at the breakpoint", &|cx| context(cx, "debugSessionPaused"));
        long(cx, "the locals", &|cx| fixture.shows_line_value("2", cx));
        let view = fixture.view(cx).expect("the Debug tool window");
        let variables = view.read_with(cx, |view, _| view.variables().clone());
        let lines = |cx: &mut VisualTestContext| variables.read_with(cx, |variables, cx| variables.variable_lines(cx));

        // Evaluate, then the same expression as a watch, which Delete removes.
        let field = variables.read_with(cx, |variables, _| variables.eval_field().clone());
        focus(cx, &field);
        cx.simulate_input("line + 1");
        cx.simulate_keystrokes("enter");
        long(cx, "the value", &|cx| lines(cx).iter().any(|row| row.contains("value of line + 1")));
        focus(cx, &field);
        field.update(cx, |field, cx| field.select_all(cx));
        cx.simulate_input("watched");
        cx.simulate_keystrokes("cmd-shift-enter");
        long(cx, "the watch", &|cx| lines(cx).iter().any(|row| row.contains("value of watched")));
        select_variable(&variables, "watched", cx);
        cx.simulate_keystrokes("delete");
        long(cx, "the watch to go", &|cx| !lines(cx).iter().any(|row| row.contains("value of watched")));

        // Set Value writes the local through the adapter.
        select_variable(&variables, "line", cx);
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        assert!(variables.read_with(cx, |variables, _| variables.is_editing()));
        let edit = variables.read_with(cx, |variables, _| variables.edit_field().clone());
        edit.update(cx, |edit, cx| edit.select_all(cx));
        cx.simulate_input("42");
        cx.simulate_keystrokes("enter");
        long(cx, "the value to be set", &|cx| {
            !variables.read_with(cx, |variables, _| variables.is_editing()) && fixture.shows_line_value("42", cx)
        });
        assert_eq!(variables.read_with(cx, |variables, _| variables.edit_error().map(str::to_owned)), None);

        // Copy Value of a structure reads its fields from the adapter.
        variables.update_in(cx, |variables, window, cx| {
            variables.open_menu_on("point", window, cx);
            variables.run_variable_command(VariableCommand::CopyValue, window, cx);
        });
        long(cx, "the copied value", &|cx| {
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
                .is_some_and(|text| text.contains('x'))
        });
        // The row's menu the command came from closes with Escape.
        assert!(fixture.workbench.read_with(cx, |w, _| w.popup.is_some()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(fixture.workbench.read_with(cx, |w, _| w.popup.is_none()));

        // Go to Source of the breakpoint closes the dialog and opens its line.
        fixture.caret_to(0, cx);
        run(cx, "debug.viewBreakpoints");
        let dialog_open =
            |cx: &mut VisualTestContext| fixture.workbench.read_with(cx, |w, _| w.breakpoints_dialog().is_some());
        long(cx, "the Breakpoints dialog", &dialog_open);
        let dialog = fixture.workbench.read_with(cx, |w, _| w.breakpoints_dialog().cloned()).expect("the dialog");
        let path = fixture.root.join("main.fake").to_string_lossy().into_owned();
        dialog.update(cx, |_, cx| cx.emit(BreakpointsDialogEvent::GoToSource { path, line: 1 }));
        long(cx, "the dialog to close", &|cx| !dialog_open(cx));
        long(cx, "the breakpoint's line", &|cx| caret_line(&fixture, cx) == 1);

        // Closing the live session's tab asks; Terminate ends the session.
        let session = active_session(&fixture, cx).expect("the session");
        view.update(cx, |_, cx| cx.emit(DebugWindowEvent::CloseTab(session)));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Terminate");
        long(cx, "the session to end", &|cx| !context(cx, "debugSessionActive"));

        close_project(&fixture, cx);
    }

    // Not from a spec: Alt in the gutter adds a temporary breakpoint; Shift adds a logging one and
    // opens the Breakpoints dialog on it.
    #[gpui_kit::test]
    async fn alt_and_shift_in_the_gutter_add_a_temporary_and_a_logging_breakpoint(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, PROGRAM);
        let dialog_open =
            |cx: &mut VisualTestContext| fixture.workbench.read_with(cx, |w, _| w.breakpoints_dialog().is_some());
        let editor = fixture.editor(cx);
        let at = |line: u32, cx: &mut VisualTestContext| {
            let at = editor.read_with(cx, |editor, _| editor.position_of(editor.offset_of(line, 0)));
            let at = at.expect("the line is painted");
            point(at.x - gpui_kit::px(FOLD_GUTTER + 12.0), at.y + gpui_kit::px(4.0))
        };
        let press = |line: u32, modifiers: Modifiers, cx: &mut VisualTestContext| {
            let position = at(line, cx);
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            });
            cx.run_until_parked();
        };
        press(2, Modifiers { alt: true, ..Modifiers::default() }, cx);
        long(cx, "the temporary breakpoint", &|cx| fixture.marks(MarkLayer::Breakpoints, cx).len() == 1);
        let temporary = fixture.driver(cx).read_with(cx, |driver, cx| {
            let lines = &driver.debugger().read(cx).breakpoints.lines;
            lines.by_file().values().flatten().any(|entry| entry.line == 2 && entry.temporary)
        });
        assert!(temporary, "the breakpoint on line 3 is temporary");
        press(0, Modifiers { shift: true, ..Modifiers::default() }, cx);
        long(cx, "the Breakpoints dialog", &dialog_open);
        let logging = fixture.driver(cx).read_with(cx, |driver, cx| {
            let lines = &driver.debugger().read(cx).breakpoints.lines;
            lines.by_file().values().flatten().any(|entry| entry.line == 0 && !entry.suspend && entry.log_hit)
        });
        assert!(logging, "the breakpoint on line 1 logs and does not suspend");
        // On a line that has a breakpoint, Shift adds nothing.
        let dialog = fixture.workbench.read_with(cx, |w, _| w.breakpoints_dialog().cloned()).expect("the dialog");
        dialog.update(cx, |_, cx| cx.emit(BreakpointsDialogEvent::Close));
        long(cx, "the dialog to close", &|cx| !dialog_open(cx));
        press(2, Modifiers { shift: true, ..Modifiers::default() }, cx);
        let logging = fixture.driver(cx).read_with(cx, |driver, cx| {
            let lines = &driver.debugger().read(cx).breakpoints.lines;
            lines.by_file().values().flatten().any(|entry| entry.line == 2 && !entry.suspend)
        });
        assert!(!logging, "Shift on a line that has a breakpoint adds none");
        close_project(&fixture, cx);
    }

    // Not from a spec: the run widget's Debug starts the selected configuration under the debugger,
    // shows "Starting…" only until the session is up, and is offered again once it ended.
    #[gpui_kit::test]
    async fn the_run_widgets_debug_button_debugs_the_selection(cx: &mut TestAppContext) {
        let (fixture, cx) = open(cx, PROGRAM);
        fixture.caret_to(1, cx);
        run(cx, "debug.toggleBreakpoint");
        fixture.select_main(cx);
        let widget = fixture.workbench.read_with(cx, |w, _| w.run_widget().cloned()).expect("the run widget");
        cx.run_until_parked();
        let bounds = cx.debug_bounds("run-widget-debug").expect("the widget offers Debug");
        cx.simulate_click(bounds.center(), Modifiers::default());
        long(cx, "the stop at the breakpoint", &|cx| context(cx, "debugSessionPaused"));
        long(cx, "the widget to leave Starting…", &|cx| !widget.read_with(cx, |widget, cx| widget.loading(cx)));
        run(cx, "debug.resume");
        long(cx, "the end of the program", &|cx| fixture.status(cx) == Some(SessionStatus::Terminated));
        let buttons = widget.read_with(cx, |widget, cx| widget.buttons(cx));
        assert!(buttons.iter().any(|b| b.id == "run-widget-debug" && b.enabled), "{buttons:?}");

        // Closed and opened again in the same window, the project debugs with a new debugger whose
        // Debug tool window is the one shown, and its breakpoint is still there.
        let first = fixture.driver(cx).entity_id();
        close_project(&fixture, cx);
        assert!(fixture.workbench.read_with(cx, |w, _| w.debug_driver().is_none()));
        let root = fixture.root.clone();
        fixture.workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
        long(cx, "the project to open again", &|cx| fixture.workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let file = root.join("main.fake");
        fixture.workbench.update_in(cx, |w, window, cx| w.open_project_file(file, window, cx));
        long(cx, "the editor", &|cx| fixture.workbench.read_with(cx, |w, _| w.editor().is_some()));
        assert_ne!(fixture.driver(cx).entity_id(), first);
        let marked = fixture.line_mark(1, MarkKind::BreakpointLine, cx);
        long(cx, "the stored breakpoint", &|cx| fixture.marks(MarkLayer::Breakpoints, cx) == [marked.clone()]);
        fixture.select_main(cx);
        run(cx, "run.debug");
        long(cx, "the stop at the breakpoint", &|cx| context(cx, "debugSessionPaused"));
        long(cx, "the frame", &|cx| fixture.frame_lines(cx) == ["main, main.fake:2"]);
        let view = fixture.view(cx).map(|view| view.entity_id());
        let shown =
            fixture.workbench.read_with(cx, |w, _| w.tool_window_view(DEBUG_TOOL_WINDOW)).map(|v| v.entity_id());
        assert_eq!(shown, view, "the Debug tool window shows the new debugger's view");
        close_project(&fixture, cx);
    }
}
