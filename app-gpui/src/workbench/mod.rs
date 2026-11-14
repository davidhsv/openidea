//! The window shell. For now: a frame around one editor (LightEdit), which is the part of the
//! workbench the editor slice needs. Tool windows, the title bar and the status bar follow.

mod agents;
mod app_actions;
mod app_shell;
pub mod background_tasks_view;
mod bookmarks_window;
use bookmarks_window::BOOKMARKS_TOOL_WINDOW;
mod branches;
mod build_window;
mod chrome;
mod code_insight;
use crate::code_insight::{HIERARCHY_TOOL_WINDOW, PROBLEMS_TOOL_WINDOW, USAGES_TOOL_WINDOW};
mod commit;
mod database;
mod debug;
use crate::database::tree_view::DATABASE_TOOL_WINDOW;
mod conflicts;
pub mod context_menus;
use crate::build_window::model::BUILD_TOOL_WINDOW;
use crate::debugger::effects::DEBUG_TOOL_WINDOW;
mod diff_window;
mod editor_core;
mod editor_extras;
mod editor_find;
mod editor_intelligence;
mod editor_language;
mod github;
mod log_actions;
mod log_window;
pub mod main_toolbar;
pub mod menu;
pub mod menu_spec;
pub mod notification_balloons;
pub mod notifications;
pub mod notifications_view;
pub mod plugin_contributions;
mod plugin_java_debug;
mod plugin_runtime;
mod plugin_tool_windows;
mod plugins_window;
pub mod project_color;
mod project_files;
mod project_view_actions;
pub(crate) use app_actions::zen_active as distraction_free_active;
pub use project_view_actions::undo_in_project_view;
pub mod quick_switch;
mod run_configs;
mod services_actions;
mod services_window;
mod settings_window;
pub use settings_window::apply_keymap_settings;
mod shelf;
mod structure_window;
pub mod tab_views;
use crate::github::window_view::PULL_REQUESTS_TOOL_WINDOW;
use structure_window::STRUCTURE_TOOL_WINDOW;
mod terminal_actions;
mod todo_window;
mod vcs_operations;
use todo_window::TODO_TOOL_WINDOW;
pub mod status_bar;
pub mod status_bar_widgets;
pub mod status_items;
pub mod tasks;
pub mod title_bar;
pub mod tool_windows;
pub mod welcome;
pub mod welcome_model;
pub mod welcome_pages;
pub mod windows;

use crate::backend::{Backend, BackendEvent};
use crate::editor::find_bar::{FindBar, scroll_to_results, toggle_scroll_to_results};
use crate::editor::goto_line::{GotoLine, GotoLineEvent};
use crate::editor::intelligence::EditorIntelligence;
use crate::editor::session::{self, FileViewState};
use crate::editor::tabs::{
    EditorInput, LayoutNode, OpenTabOptions, SplitDir, SplitEdge, TabsState, disambiguate_titles,
};
use crate::editor::view::Editor;
use crate::everywhere::bookmarks_popup::{
    BookmarksPopup, BookmarksPopupEvent, MnemonicChooser, MnemonicChooserEvent, neighbour_bookmark,
    selection_description,
};
use crate::everywhere::bookmarks_store::{
    BookmarkStore, MnemonicTarget, bookmark_by_mnemonic, bookmarks_key, lists_key, toggle_bookmark, toggle_mnemonic,
    toggle_path_bookmarks,
};
use crate::everywhere::model::SeTab;
use crate::everywhere::open::StorageWrite;
use crate::everywhere::recent_model::OpenEditorTab;
use crate::everywhere::recent_view::{RecentFiles, RecentFilesEvent, RecentFilesInput, RecentFilesKind};
use crate::everywhere::structure_view::{FileStructure, FileStructureEvent, StructureFile};
use crate::everywhere::view::{SearchEverywhere, SearchEverywhereEvent};
use crate::find::popup_view::{FindPopup, FindPopupEvent, FindPopupInput};
use crate::find::store::{FindArg, FindPopupCommand, parse_find_arg};
use crate::platform::actions::{Action, ActionMeta, ActionRegistry, ActionSource, execute_action_from, manifest};
use crate::platform::input;
use crate::platform::local_time::now_ms;
use crate::platform::storage::UiStorage;
use crate::project_tree::{OpenFile, ProjectTree};
use crate::run::driver::{RunDriver, RunDriverEvent, SaveAll};
use crate::run::sessions::{StopTarget, rerun_target, stop_target};
use crate::run::store::RunStore;
use crate::run::view::{RunView, RunViewEvent};
use crate::theme;
use crate::ui::popup_menu::{Choice, PopupMenu, PopupMenuEvent};
use crate::ui::tooltip::WithTooltip;
use crate::vcs_ops::menu::MenuItem;
use editor_find::TabFindBar;
use gpui_kit::Subscription;
use gpui_kit::{AnyView, FocusHandle};
use gpui_kit::{
    App, Context, Entity, Focusable, Global, IntoElement, ModifiersChangedEvent, MouseDownEvent, Render, SharedString,
    WeakEntity, Window, WindowId, div, prelude::*,
};
use ide_core::IdeEvent;
use ide_core::event::NotifyLevel;
use notification_balloons::{BalloonEvent, EntryAction, NotificationBalloons, balloon_entries};
use notifications::{
    BalloonClocks, BalloonView, NotificationLevel, NotificationRequest, NotificationSettings, Notifications,
    notification_settings, suppress_group,
};
use notifications_view::{NotificationsEvent, NotificationsView, logged_entries};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use tasks::{TaskEvent, Tasks};
use tool_windows::{Anchor, Effects, Slot, Stripe, ToolWindow, ToolWindowLayout, storage_key};
use welcome::{Welcome, WelcomeEvent};

/// What a window shows.
#[expect(clippy::large_enum_variant, reason = "one per window, replaced only when a project opens or closes")]
enum Content {
    /// Nothing is open.
    Empty,
    Opening(PathBuf),
    Editor(Entity<Editor>),
    /// A project: its file tree and the file being edited.
    Project {
        name: String,
        /// Which tool windows are shown, where and how large.
        layout: Box<ToolWindowLayout>,
        /// The last focus request of `layout` that was carried out.
        focus_served: u64,
        tree: Entity<ProjectTree>,
        /// The view of every tool window that has been shown, by tool window id. A hidden
        /// window keeps its view, and what runs in it.
        panels: HashMap<String, Panel>,
        /// The editor tabs, and the editor of each tab that has finished opening, by tab id.
        tabs: Box<TabsState>,
        editors: HashMap<String, Entity<Editor>>,
        /// Tabs whose document is being opened.
        loading: std::collections::HashSet<String>,
        /// Where the files of the restored session were left, for the ones not opened yet.
        restored_views: std::collections::BTreeMap<String, FileViewState>,
        /// Where to put the caret in a file that is being opened: its line and UTF-16 column.
        reveals: HashMap<String, (u32, u32)>,
        /// What tells the window that a tab's editor took the keyboard, by tab id: its pane is
        /// the active one from then on.
        editor_focus: HashMap<String, Subscription>,
        /// The Find / Replace bar of each tab that has one open, by tab id.
        find_bars: HashMap<String, TabFindBar>,
        /// The views of the tabs that are not text editors, the Markdown layouts of text tabs,
        /// and the probes choosing between a text editor and the hex view, by tab id.
        view_tabs: tab_views::ViewTabs,
        _opens: Subscription,
        /// What runs the project's run configurations, and what the window listens to it for.
        run: Entity<RunDriver>,
        _run_events: Subscription,
    },
    /// A loose file shown in a viewer (an image, binary content) instead of a text editor.
    Viewer(Box<tab_views::TabView>),
    Failed(SharedString),
}

/// A tool window's content and where its keyboard focus goes.
struct Panel {
    view: AnyView,
    /// Where the keyboard goes when the window is given it: asked each time, since a view may
    /// hand it on to a part of itself that changes (the terminal's active tab).
    focus: Rc<dyn Fn(&App) -> FocusHandle>,
    /// What the window listens to the view for, kept for as long as the view is.
    _events: Option<Subscription>,
}

/// The longest editor selection, in UTF-16 code units, that Search Everywhere starts with.
const SELECTION_QUERY_LIMIT: usize = 200;

/// What a row of an editor tab's menu does to the tab it was opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabCommand {
    Close,
    CloseOthers,
    CloseLeft,
    CloseRight,
    CloseAll,
    CloseUnmodified,
    CloseAllButPinned,
    CloseReadOnly,
    TogglePin,
    Keep,
    SplitAndMoveRight,
    SplitAndMoveDown,
    SplitRight,
    SplitDown,
    MoveToOpposite,
    OpenInOpposite,
    ChangeOrientation,
    Unsplit,
    UnsplitAll,
}

/// The encodings a file can be reloaded in or converted to, with the names they are shown under.
pub const ENCODINGS: [(&str, &str); 11] = [
    ("utf-8", "UTF-8"),
    ("utf-16le", "UTF-16LE"),
    ("utf-16be", "UTF-16BE"),
    ("gbk", "GBK"),
    ("gb18030", "GB18030"),
    ("big5", "Big5"),
    ("shift_jis", "Shift_JIS"),
    ("euc-kr", "EUC-KR"),
    ("windows-1252", "windows-1252"),
    ("iso-8859-1", "ISO-8859-1"),
    ("koi8-r", "KOI8-R"),
];

/// What a row of a status bar item's menu does to the file being edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusCommand {
    LineEnding(ide_text::LineEnding),
    /// Read the file again in this encoding.
    Reload(String),
    /// Keep the text and save it in this encoding.
    Convert(String),
}

/// A popup over the window and where the keyboard goes when it closes.
struct Popup {
    view: AnyView,
    /// What had the keyboard when the popup opened.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

/// How long the tabs rest before the editor session is written.
const SESSION_SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

impl Panel {
    fn new<V: Focusable + Render>(view: Entity<V>, events: Option<Subscription>) -> Self {
        let focused = view.clone();
        Self { view: view.into(), focus: Rc::new(move |cx| focused.focus_handle(cx)), _events: events }
    }
}

/// One window: its backend label and what it shows.
pub struct Workbench {
    /// The label the backend knows this window by.
    label: String,
    content: Content,
    /// Redraws the status bar when the shown editor changes.
    editor_observer: Option<Subscription>,
    /// The title the window currently carries.
    shown_title: Option<String>,
    /// Background work the backend reports for this window, and what it tells the user.
    tasks: Tasks,
    notifications: Notifications,
    /// Wakes the task list when its next deadline (a finished task leaving, a slow one
    /// appearing) falls due.
    task_timer: Option<gpui_kit::Task<()>>,
    listener: u64,
    /// The editor that was active before the last tab change: it is saved when another takes over.
    previous_editor: Option<gpui_kit::WeakEntity<Editor>>,
    /// The window's close requests: one is decided at a time.
    close_requests: crate::app_models::close_guard::CloseRequests,
    /// The next [`Workbench::leave`] follows a close decision, which settled the unsaved documents.
    leave_decided: bool,
    /// The close or open question shown over the window.
    app_dialog: Option<app_shell::AppDialog>,
    /// A pending write of the editor session.
    session_save: Option<gpui_kit::Task<()>>,
    /// The popup that is open over everything else in the window.
    popup: Option<Popup>,
    /// When each tab's editor was last the active one, by the key of what it shows: a count that
    /// grows with every change of the active editor. The Switcher lists the open editors by it.
    activations: HashMap<String, u64>,
    /// The key of the editor that is active, to tell a change from a repeat.
    active_key: Option<String>,
    /// The bookmarks of the window's project.
    bookmarks: BookmarkStore,
    /// The balloons at the bottom right of the window, and what times their fading.
    balloons: Entity<NotificationBalloons>,
    balloon_clocks: BalloonClocks,
    balloon_view: BalloonView,
    /// Wakes the balloons when the next one is due to fade.
    balloon_timer: Option<gpui_kit::Task<()>>,
    _balloon_events: Subscription,
    /// The Welcome screen, while the window shows nothing, and what the window listens to it for.
    welcome: Option<(Entity<Welcome>, Subscription)>,
    /// The Find / Replace bar of a single-file window's editor.
    light_find_bar: Option<TabFindBar>,
    /// Where the caret has been in the window's editors: Back / Forward and the edit locations.
    editor_core: editor_core::EditorCore,
    /// The Services tool window of the open project, once it was shown or used.
    services: Option<services_window::ServicesWindow>,
    /// The diff shown in front of the window.
    diff_window: Option<diff_window::DiffWindow>,
    /// The run widget of the project's title bar.
    run_widget: Option<run_configs::RunWidgetSlot>,
    /// Edit Configurations, while it is open.
    edit_configs: Option<run_configs::EditConfigsWindow>,
    /// The Settings dialog, while it is open.
    settings: Option<settings_window::SettingsWindow>,
    /// The window's plugin host, the Welcome screen's Plugins page, the page of the Plugins actions
    /// and the dialog they show.
    plugin_ui: plugins_window::PluginsSlot,
    /// The Git state of the project, with the branch widget of the title bar.
    vcs: Option<branches::VcsSlot>,
    /// The Pull Requests tool window of the project: the GitHub connection and the opened pull requests.
    github: Option<github::GithubSlot>,
    /// The commit draft of the project and the Commit tool window's message box.
    commit: Option<commit::CommitSlot>,
    /// The Git tool window's log and the details pane beside it.
    git_log: Option<log_window::LogSlot>,
    /// The dialog a Git operation waits on.
    vcs_dialog: Option<branches::VcsDialog>,
    /// The editor language actions: the refactorings undoable as one step, the highlighted usages,
    /// the rename waiting for its edit.
    language: editor_language::EditorLanguage,
    /// The status bar's transient message (Next Occurrence's "2 of 5").
    extras: editor_extras::EditorExtras,
    /// Resolving the project's merge conflicts: the merge editor, the conflict banners and what the
    /// Conflicts dialog remembers.
    conflicts: conflicts::ConflictsSlot,
    /// The Git menu's repository operations: Show Commit in Git Log waiting for the log.
    vcs_operations: vcs_operations::VcsOperationsSlot,
    /// The project's change lists and shelf.
    shelf: shelf::ShelfSlot,
    /// What the Git log's actions keep for their notifications' buttons (Undo of a history rewrite).
    log_actions: log_actions::LogActionsSlot,
    /// The editor the Structure tool window follows.
    structure: Option<structure_window::StructureFeed>,
    /// What the TODO tool window is fed from.
    todo: Option<todo_window::TodoFeed>,
    /// The project's build.
    build: Option<build_window::BuildSlot>,
    /// The Project view's file operations.
    files: project_files::ProjectFiles,
    /// The Database tools of the open project, once shown or used.
    database: Option<database::DatabaseSlot>,
    /// Problems, Find / Show Usages, the hierarchies and the language servers of the project.
    code_insight: code_insight::CodeInsight,
    /// The project's agent tasks: their store, the subscription to their events and the conversation
    /// docked in the Agents tool window.
    agents: Option<agents::AgentsSlot>,
    /// The debugger of the open project: its driver, the Debug tool window and the Breakpoints dialog.
    debug: Option<debug::DebugSlot>,
    /// The title bar and the status bar, and what the window keeps for them.
    chrome: chrome::Chrome,
    /// What running plugin code shows in the window, and what the window tells the plugins.
    plugin_runtime: plugin_runtime::PluginRuntime,
    /// Plugins' gutter actions, activation events and what else they contribute to the window.
    plugin_contributions: plugin_contributions::PluginContributions,
    /// The tool windows of the views plugins declare, their panels and what they wait on.
    plugin_tool_windows: plugin_tool_windows::PluginToolWindows,
}

/// Window title, IDEA style: `<project> – <file>`; with "Always show full path in window header"
/// the file's full path. A remote project names its host: `<project> [<user@host>] – <file>`.
/// Without a project the file alone, or `default` when nothing is open.
pub fn window_title(
    project: Option<&str>,
    file: Option<&std::path::Path>,
    full_path: bool,
    host: Option<&str>,
    default: &str,
) -> String {
    let name = file.map(|file| match file.file_name().filter(|_| !full_path) {
        Some(name) => name.to_string_lossy().into_owned(),
        None => file.to_string_lossy().into_owned(),
    });
    let Some(project) = project.filter(|p| !p.is_empty()) else { return name.unwrap_or_else(|| default.to_owned()) };
    let place = match host {
        Some(host) => format!("{project} [{host}]"),
        None => project.to_owned(),
    };
    match name {
        Some(name) => format!("{place} – {name}"),
        None => place,
    }
}

/// What a tab shows: the file's name, the folder that tells it apart from a tab of the same
/// name, and whether it is the selected one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabTitle {
    pub name: String,
    pub hint: Option<String>,
    pub active: bool,
}

/// How a workbench window is created: the title bar is the app's own, drawn under the native
/// window buttons (an overlay title bar).
pub fn window_options() -> gpui_kit::WindowOptions {
    gpui_kit::WindowOptions {
        titlebar: Some(gpui_kit::TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: Some(gpui_kit::point(gpui_kit::px(13.0), gpui_kit::px(13.0))),
        }),
        ..Default::default()
    }
}

/// Room the title bar leaves for the native macOS window buttons.
const WINDOW_BUTTONS_WIDTH: f32 = 82.0;

/// Where the reopen-last-project preference of this machine is kept (`"false"`: off).
const REOPEN_PREFERENCE: &str = "openidea.welcome.reopenLastProject";

/// The Project view's tool window id.
pub const PROJECT_TOOL_WINDOW: &str = "project";

/// The Terminal's tool window id.
pub const TERMINAL_TOOL_WINDOW: &str = "terminal";

/// The Git log's tool window id.
pub const GIT_TOOL_WINDOW: &str = "git";

/// The Find tool window's id.
pub const FIND_TOOL_WINDOW: &str = "find";

/// The Commit tool window's id.
pub const COMMIT_TOOL_WINDOW: &str = "commit";

/// The Notifications tool window's id.
pub const NOTIFICATIONS_TOOL_WINDOW: &str = "notifications";

/// The Run tool window's id.
pub const RUN_TOOL_WINDOW: &str = "run";

pub use crate::services::model::SERVICES_TOOL_WINDOW;

/// The tool windows this app has a view for.
const TOOL_WINDOWS: [&str; 19] = [
    PROJECT_TOOL_WINDOW,
    COMMIT_TOOL_WINDOW,
    TERMINAL_TOOL_WINDOW,
    GIT_TOOL_WINDOW,
    FIND_TOOL_WINDOW,
    NOTIFICATIONS_TOOL_WINDOW,
    RUN_TOOL_WINDOW,
    SERVICES_TOOL_WINDOW,
    BOOKMARKS_TOOL_WINDOW,
    STRUCTURE_TOOL_WINDOW,
    TODO_TOOL_WINDOW,
    BUILD_TOOL_WINDOW,
    PROBLEMS_TOOL_WINDOW,
    USAGES_TOOL_WINDOW,
    HIERARCHY_TOOL_WINDOW,
    crate::agents::model::TOOL_WINDOW,
    DATABASE_TOOL_WINDOW,
    DEBUG_TOOL_WINDOW,
    PULL_REQUESTS_TOOL_WINDOW,
];

/// What the contributions manifest declares for tool window `id` (title, icon, side, order…).
pub fn tool_window_declaration(id: &str) -> Option<ToolWindow> {
    static DECLARED: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    let manifest = DECLARED.get_or_init(|| {
        serde_json::from_str(include_str!("../../assets/gen/contributions.json"))
            .expect("assets/gen/contributions.json is generated and valid")
    });
    let declared = manifest
        .get("registerToolWindow")?
        .as_array()?
        .iter()
        .find(|w| w.get("id").and_then(|v| v.as_str()) == Some(id))?;
    let text = |key: &str| declared.get(key).and_then(|v| v.as_str());
    let anchor = match text("anchor")? {
        "left" => Anchor::Left,
        "right" => Anchor::Right,
        "bottom" => Anchor::Bottom,
        _ => return None,
    };
    let flag = |key: &str| declared.get(key).and_then(|v| v.as_bool());
    Some(ToolWindow {
        order: declared.get("order").and_then(|v| v.as_f64()),
        secondary: flag("secondary").unwrap_or(false),
        stripe: if text("stripe") == Some("onUse") { Stripe::OnUse } else { Stripe::Shown },
        verbatim: flag("verbatim").unwrap_or(false),
        requires_project: flag("requiresProject").unwrap_or(true),
        ..ToolWindow::new(id, text("title")?, text("icon")?, anchor)
    })
}

/// Registers the workbench's actions: one `toolwindow.<id>` per tool window this app has a view
/// for, and the window-level ones (hide all, jump to last, hide active, focus editor).
pub fn register_actions(cx: &mut App) {
    fn on_layout(
        window: &mut Window,
        cx: &mut App,
        change: impl FnOnce(&mut ToolWindowLayout) -> Effects,
    ) -> anyhow::Result<()> {
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        workbench.update(cx, |workbench, cx| workbench.with_layout(window, cx, change));
        Ok(())
    }
    windows::register_actions(cx);
    let registry = cx.default_global::<ActionRegistry>();
    services_window::register_actions(registry);
    github::register_actions(registry);
    diff_window::register_actions(registry);
    run_configs::register_actions(registry);
    debug::register_actions(registry);
    branches::register_actions(registry);
    conflicts::register_actions(registry);
    vcs_operations::register_actions(registry);
    shelf::register_actions(registry);
    commit::register_actions(registry);
    log_window::register_actions(registry);
    log_actions::register_actions(registry);
    bookmarks_window::register_actions(registry);
    project_files::register_actions(registry);
    editor_core::register_actions(registry);
    todo_window::register_actions(registry);
    project_view_actions::register_actions(registry);
    terminal_actions::register_actions(registry);
    build_window::register_actions(registry);
    database::register_actions(registry);
    code_insight::register_actions(registry);
    editor_language::register_actions(registry);
    editor_extras::register_actions(registry);
    agents::register_actions(registry);
    tab_views::register_actions(registry);
    plugin_runtime::register_actions(registry);
    plugin_contributions::register_actions(registry);
    chrome::register_actions(registry);
    settings_window::register_actions(registry);
    app_actions::register_actions(registry);
    plugins_window::register_actions(registry);
    // View › Appearance: each item shows and flips one global appearance setting.
    type Flag = fn(&ide_core::settings::AppearanceSettings) -> bool;
    let flags: [(&str, &str, Flag); 4] = [
        ("view.toggleCompactMode", "appearance.compactMode", |a| a.compact_mode),
        ("view.toggleToolbar", "appearance.showMainToolbar", |a| a.show_main_toolbar),
        ("view.toggleToolWindowBars", "appearance.showToolWindowBars", |a| a.show_tool_window_bars),
        ("view.toggleStatusBar", "appearance.showStatusBar", |a| a.show_status_bar),
    ];
    for (id, key, flag) in flags {
        registry.handle_toggle(
            id,
            move |_, cx| flag(&crate::platform::settings::global(cx).appearance),
            move |_, _, cx| {
                use ide_workspace::settings::SettingsScope;
                let next = !flag(&crate::platform::settings::global(cx).appearance);
                crate::platform::settings::set("", SettingsScope::Global, key, &next.into(), cx)?;
                Ok(())
            },
        );
    }
    registry.handle("toolwindow.maximize", |_, window, cx| {
        on_layout(window, cx, |layout| match layout.focused().or(layout.last_active()).map(str::to_owned) {
            Some(id) => layout.toggle_maximize(&id),
            None => Effects::default(),
        })
    });
    use tool_windows::ResizeDirection;
    // Their ids are built from the direction, so they are not in the generated list of actions.
    for (id, title, direction) in [
        ("toolwindow.resizeLeft", "Stretch to Left", ResizeDirection::Left),
        ("toolwindow.resizeRight", "Stretch to Right", ResizeDirection::Right),
        ("toolwindow.resizeUp", "Stretch to Top", ResizeDirection::Up),
        ("toolwindow.resizeDown", "Stretch to Bottom", ResizeDirection::Down),
    ] {
        let meta = ActionMeta {
            category: Some("Window".to_owned()),
            keywords: ["resize tool window", "tool window size"].map(str::to_owned).to_vec(),
            ..ActionMeta::new(id, title).when("toolWindowFocus")
        };
        registry.register(Action::new(meta, move |_, window, cx| {
            let size = window.viewport_size();
            on_layout(window, cx, |layout| {
                layout.resize_active(direction, f64::from(f32::from(size.width)), f64::from(f32::from(size.height)))
            })
        }));
    }
    for id in TOOL_WINDOWS {
        let Some(declared) = tool_window_declaration(id) else { continue };
        let meta = ActionMeta {
            category: Some("Tool Windows".to_owned()),
            icon: Some(declared.icon.clone()),
            keywords: ["tool window", "show", "hide"].map(str::to_owned).to_vec(),
            verbatim: declared.verbatim,
            ..ActionMeta::new(&format!("toolwindow.{id}"), &declared.title)
        };
        registry.register(Action::new(meta, move |_, window, cx| on_layout(window, cx, |layout| layout.activate(id))));
    }
    fn on_tabs(
        window: &mut Window,
        cx: &mut App,
        change: impl FnOnce(&mut TabsState, &str) -> bool,
    ) -> anyhow::Result<()> {
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        workbench.update(cx, |workbench, cx| workbench.with_tabs(window, cx, change));
        Ok(())
    }
    /// The active tab of `pane`, for actions that act on it.
    fn active(tabs: &TabsState, pane: &str) -> Option<String> {
        tabs.pane(pane).and_then(|pane| pane.active.clone())
    }
    use crate::terminal::panel::TerminalPanel;
    fn on_terminals(
        window: &mut Window,
        cx: &mut App,
        act: impl FnOnce(&mut TerminalPanel, &mut Context<TerminalPanel>),
    ) -> anyhow::Result<()> {
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        let terminals = workbench
            .read(cx)
            .tool_window_view(TERMINAL_TOOL_WINDOW)
            .and_then(|view| view.downcast::<TerminalPanel>().ok())
            .ok_or_else(|| anyhow::anyhow!("the Terminal has not been opened"))?;
        terminals.update(cx, act);
        Ok(())
    }
    registry.handle("terminal.new", |_, window, cx| {
        // A Terminal that exists gets the tab first and is shown then. One that this shows for
        // the first time starts with a tab of its own, which is the new tab, unless it came
        // back with remembered tabs: then the new one is added to them.
        let existed = on_terminals(window, cx, |terminals, cx| terminals.open_tab(cx)).is_ok();
        on_layout(window, cx, |layout| layout.show(TERMINAL_TOOL_WINDOW, true))?;
        if !existed {
            on_terminals(window, cx, |terminals, cx| {
                if !terminals.take_fresh() {
                    terminals.open_tab(cx);
                }
            })?;
        }
        // The keyboard goes to the tab that was opened.
        on_layout(window, cx, |layout| layout.focus(TERMINAL_TOOL_WINDOW))
    });
    registry.handle("terminal.clear", |_, window, cx| {
        on_terminals(window, cx, |terminals, cx| {
            if let Some(terminal) = terminals.active_view().cloned() {
                terminal.update(cx, |terminal, cx| terminal.clear(cx));
            }
        })
    });
    for (id, tab) in [
        ("navigate.searchEverywhere", SeTab::All),
        ("navigate.gotoClass", SeTab::Classes),
        ("navigate.gotoFile", SeTab::Files),
        ("navigate.gotoSymbol", SeTab::Symbols),
        ("navigate.findAction", SeTab::Actions),
        ("navigate.searchText", SeTab::Text),
    ] {
        registry.handle(id, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| workbench.show_search_everywhere(tab, window, cx));
            Ok(())
        });
    }
    for (id, kind) in
        [("navigate.recentFiles", None), ("navigate.switcher", Some(false)), ("navigate.switcherBack", Some(true))]
    {
        registry.handle(id, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            let kind = match kind {
                None => RecentFilesKind::Recent,
                Some(backward) => RecentFilesKind::Switcher { backward, control_held: window.modifiers().control },
            };
            workbench.update(cx, |workbench, cx| workbench.show_recent_files(kind, window, cx));
            Ok(())
        });
    }
    type Shown = fn(&mut Workbench, &mut Window, &mut Context<Workbench>);
    let shown: [(&str, Shown); 5] = [
        ("navigate.gotoLine", Workbench::show_goto_line),
        ("navigate.showBookmarks", Workbench::show_bookmarks),
        ("navigate.toggleBookmarkWithMnemonic", Workbench::show_mnemonic_chooser),
        ("navigate.nextBookmark", |workbench, window, cx| workbench.go_to_neighbour_bookmark(true, window, cx)),
        ("navigate.prevBookmark", |workbench, window, cx| workbench.go_to_neighbour_bookmark(false, window, cx)),
    ];
    for (id, show) in shown {
        registry.handle(id, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| show(workbench, window, cx));
            Ok(())
        });
    }
    registry.handle("navigate.toggleBookmark", |call, window, cx| {
        let place = call.arg.and_then(|arg| {
            Some((arg.get("path")?.as_str()?.to_owned(), u32::try_from(arg.get("line")?.as_u64()?).ok()?))
        });
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        workbench.update(cx, |workbench, cx| workbench.toggle_bookmark(place, cx));
        Ok(())
    });
    registry.handle("bookmarks.toggleFileBookmark", |call, window, cx| {
        let targets: Vec<(String, bool)> = call
            .arg
            .and_then(|arg| arg.get("targets")?.as_array())
            .into_iter()
            .flatten()
            .filter_map(|target| {
                let dir = target.get("dir").and_then(serde_json::Value::as_bool).unwrap_or(false);
                Some((target.get("path")?.as_str()?.to_owned(), dir))
            })
            .collect();
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        workbench.update(cx, |workbench, cx| workbench.toggle_file_bookmarks(targets, cx));
        Ok(())
    });
    // One pair of actions per digit: their ids are built from it, so they are not in the
    // generated list of actions.
    for digit in '0'..='9' {
        let toggle = ActionMeta {
            category: Some("Bookmarks".to_owned()),
            ..ActionMeta::new(&format!("navigate.toggleBookmark{digit}"), &format!("Toggle Bookmark {digit}"))
                .when("projectOpen && textEditorActive")
        };
        registry.register(Action::new(toggle, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| workbench.toggle_bookmark_digit(digit, cx));
            Ok(())
        }));
        let go = ActionMeta {
            category: Some("Bookmarks".to_owned()),
            ..ActionMeta::new(&format!("navigate.gotoBookmark{digit}"), &format!("Go to Bookmark {digit}"))
                .when("projectOpen")
        };
        registry.register(Action::new(go, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| workbench.go_to_bookmark(digit, window, cx));
            Ok(())
        }));
    }
    fn on_run(
        window: &mut Window,
        cx: &mut App,
        act: impl FnOnce(&mut RunDriver, &mut Context<RunDriver>),
    ) -> anyhow::Result<()> {
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        let driver = workbench.read(cx).run_driver().cloned().ok_or_else(|| anyhow::anyhow!("no project is open"))?;
        driver.update(cx, act);
        Ok(())
    }
    registry.handle("run.run", |_, window, cx| on_run(window, cx, |driver, cx| driver.launch_selected(cx).detach()));
    registry.handle("run.runConfig", |call, window, cx| {
        let arg = call.arg.cloned();
        on_run(window, cx, move |driver, cx| {
            driver.when_loaded(
                move |driver, cx| {
                    if let Some(config) = driver.config_from_arg(arg.as_ref(), cx) {
                        driver.start(config, Default::default(), cx).detach();
                    }
                },
                cx,
            )
        })
    });
    registry.handle("run.rerun", |_, window, cx| {
        on_run(window, cx, |driver, cx| {
            let target = rerun_target(driver.store().read(cx)).map(str::to_owned);
            if let Some(rerun) = target.and_then(|session| driver.rerun(&session, cx)) {
                rerun.detach();
            }
        })
    });
    registry.handle("run.stop", |call, window, cx| {
        let focused = call.ctx.is_truthy("toolWindowFocus")
            && matches!(
                call.ctx.get("activeToolWindow"),
                Some(crate::platform::context::ContextValue::Text(id)) if &**id == RUN_TOOL_WINDOW
            );
        let from_widget = call.arg.and_then(serde_json::Value::as_str) == Some("widget");
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        workbench.update(cx, |workbench, cx| workbench.stop_runs(focused, from_widget, window, cx));
        Ok(())
    });
    registry.handle("run.stopProcess", |call, window, cx| {
        let Some(run) = call.arg.and_then(serde_json::Value::as_u64).and_then(|run| u32::try_from(run).ok()) else {
            return Ok(());
        };
        on_run(window, cx, move |driver, cx| driver.stop_run(ide_core::RunId(run), cx))
    });
    registry.handle("run.reloadConfigs", |_, window, cx| {
        on_run(window, cx, |driver, cx| {
            if let Some(root) = driver.store().read(cx).project_root.clone() {
                driver.load(&root, cx);
            }
        })
    });
    registry.handle("view.notifications", |_, window, cx| {
        on_layout(window, cx, |layout| layout.activate(NOTIFICATIONS_TOOL_WINDOW))
    });
    registry.handle("navigate.fileStructure", |_, window, cx| {
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        workbench.update(cx, |workbench, cx| workbench.show_file_structure(window, cx));
        Ok(())
    });
    for (id, replace) in [("find.inPath", false), ("find.replaceInPath", true)] {
        registry.handle(id, move |call, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            let arg = parse_find_arg(call.arg, call.ctx);
            workbench.update(cx, |workbench, cx| workbench.show_find_popup(replace, arg, window, cx));
            Ok(())
        });
    }
    for (id, replace) in [("find.inFile", false), ("find.replaceInFile", true)] {
        registry.handle(id, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| workbench.show_find_bar(replace, window, cx));
            Ok(())
        });
    }
    for (id, forward) in [("find.next", true), ("find.previous", false)] {
        registry.handle(id, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| workbench.find_again(forward, window, cx));
            Ok(())
        });
    }
    registry.handle("find.openInWindow", |_, window, cx| {
        let bar = for_window(window, cx).and_then(|workbench| workbench.read(cx).focused_find_bar(window, cx));
        if let Some(bar) = bar {
            bar.update(cx, FindBar::open_results);
        }
        Ok(())
    });
    registry.handle_toggle(
        "find.scrollToResults",
        |_, cx| scroll_to_results(cx),
        |_, _, cx| {
            toggle_scroll_to_results(cx);
            Ok(())
        },
    );
    for (id, command) in [
        ("find.popup.nextResult", FindPopupCommand::NextResult),
        ("find.popup.previousResult", FindPopupCommand::PreviousResult),
        ("find.popup.openResult", FindPopupCommand::OpenResult),
        ("find.popup.recentSearches", FindPopupCommand::RecentSearches),
    ] {
        registry.handle(id, move |_, window, cx| {
            let popup = for_window(window, cx).and_then(|workbench| workbench.read(cx).find_popup());
            if let Some(popup) = popup {
                popup.update(cx, |popup, cx| popup.run_command(command, window, cx));
            }
            Ok(())
        });
    }
    registry.handle("view.toggleTheme", |_, window, cx| {
        let label = for_window(window, cx).map(|workbench| workbench.read(cx).label.clone()).unwrap_or_default();
        crate::theme::appearance::toggle_theme(&label, cx)
    });
    registry.handle_toggle(
        "ide.toggleMode",
        |window, cx| for_window(window, cx).is_some_and(|workbench| workbench.read(cx).ide_mode(cx).is_full()),
        |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| workbench.toggle_ide_mode(cx));
            Ok(())
        },
    );
    registry.handle("editor.closeTab", |_, window, cx| {
        on_tabs(window, cx, |tabs, pane| active(tabs, pane).is_some_and(|tab| tabs.close_tab(pane, &tab)))
    });
    registry.handle("editor.closeOtherTabs", |_, window, cx| {
        on_tabs(window, cx, |tabs, pane| active(tabs, pane).is_some_and(|tab| tabs.close_others(pane, &tab)))
    });
    registry.handle("editor.closeAllTabs", |_, window, cx| {
        on_tabs(window, cx, |tabs, _| {
            let panes: Vec<String> = tabs.pane_ids().into_iter().map(str::to_owned).collect();
            let mut changed = false;
            for pane in &panes {
                changed |= tabs.close_all(pane, false);
            }
            // Closing everything is not undone tab by tab: Reopen Closed Tab has nothing after it.
            tabs.closed.clear();
            changed
        })
    });
    registry.handle("editor.pinTab", |_, window, cx| {
        on_tabs(window, cx, |tabs, pane| active(tabs, pane).is_some_and(|tab| tabs.toggle_pin(pane, &tab)))
    });
    registry.handle("editor.nextTab", |_, window, cx| on_tabs(window, cx, |tabs, _| tabs.cycle_tab(1, None)));
    registry.handle("editor.prevTab", |_, window, cx| on_tabs(window, cx, |tabs, _| tabs.cycle_tab(-1, None)));
    type OnWorkbench = fn(&mut Workbench, &mut Window, &mut Context<Workbench>);
    let on_panes: [(&str, OnWorkbench); 8] = [
        ("editor.splitRight", |workbench, window, cx| workbench.split_editor(SplitDir::Row, window, cx)),
        ("editor.splitDown", |workbench, window, cx| workbench.split_editor(SplitDir::Column, window, cx)),
        ("editor.moveToOppositeGroup", Workbench::move_tab_to_opposite_group),
        ("editor.openInOppositeGroup", Workbench::open_in_opposite_group),
        ("editor.nextSplitter", |workbench, window, cx| workbench.focus_splitter(true, window, cx)),
        ("editor.prevSplitter", |workbench, window, cx| workbench.focus_splitter(false, window, cx)),
        ("editor.unsplit", |workbench, window, cx| {
            workbench.with_tabs(window, cx, |tabs, pane| tabs.remove_pane(pane))
        }),
        ("editor.unsplitAll", |workbench, window, cx| {
            workbench.with_tabs(window, cx, |tabs, pane| tabs.unsplit_all(pane))
        }),
    ];
    for (id, act) in on_panes {
        registry.handle(id, move |_, window, cx| {
            let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
            workbench.update(cx, |workbench, cx| act(workbench, window, cx));
            Ok(())
        });
    }
    registry.handle("editor.changeSplitOrientation", |_, window, cx| {
        on_tabs(window, cx, |tabs, pane| tabs.change_split_orientation(pane))
    });
    registry.handle("editor.reopenClosedTab", |_, window, cx| {
        let workbench = for_window(window, cx).ok_or_else(|| anyhow::anyhow!("this window has no workbench"))?;
        workbench.update(cx, |workbench, cx| workbench.reopen_closed_tab(window, cx));
        Ok(())
    });
    // In a window showing the Welcome screen the keyboard goes to its search; a window with a
    // project has no Recent Projects popup to show.
    for id in ["welcome.recentProjects", "file.openRecent"] {
        registry.handle(id, |_, window, cx| {
            let welcome = for_window(window, cx).and_then(|workbench| workbench.read(cx).welcome().cloned());
            if let Some(welcome) = welcome {
                welcome.update(cx, |view, cx| view.focus_search(window, cx));
            }
            Ok(())
        });
    }
    debug_assert!(manifest().contains_key("toolwindow.hideAll"));
    registry.handle("toolwindow.hideAll", |_, window, cx| on_layout(window, cx, ToolWindowLayout::hide_all));
    registry.handle("toolwindow.jumpToLast", |_, window, cx| on_layout(window, cx, ToolWindowLayout::jump_to_last));
    registry.handle("toolwindow.hideActive", |_, window, cx| on_layout(window, cx, ToolWindowLayout::hide_active));
    registry.handle("toolwindow.focusEditor", |_, window, cx| {
        on_layout(window, cx, |_| Effects { persist: false, focus_editor: true })
    });
}

/// The workbench of each window, so that an action can find what it acts on.
#[derive(Default)]
struct Workbenches(HashMap<WindowId, WeakEntity<Workbench>>);

impl Global for Workbenches {}

/// The workbench shown in `window`.
pub fn for_window(window: &Window, cx: &App) -> Option<Entity<Workbench>> {
    cx.try_global::<Workbenches>()?.0.get(&window.window_handle().window_id())?.upgrade()
}

/// The editor an action invoked in `window` acts on.
pub fn active_editor(window: &Window, cx: &App) -> Option<Entity<Editor>> {
    for_window(window, cx)?.read(cx).editor().cloned()
}

pub(crate) use editor_language::{redo_refactoring, undo_refactoring};

impl Workbench {
    /// The workbench of `window`, known to the backend as `label`.
    pub fn new(label: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Closing the window is decided first: the exit, the running processes, the unsaved documents.
        // The hook reads every window's workbench, this one's included, so it runs outside an update.
        let guard = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| app_shell::window_should_close(&guard, window, cx));
        cx.observe_window_appearance(window, |_, window, cx| {
            crate::theme::appearance::system_appearance_changed(window.appearance(), cx);
        })
        .detach();
        // Leaving the window saves what is unsaved, as switching tabs does.
        cx.observe_window_activation(window, |workbench, window, cx| {
            if !window.is_window_active() {
                workbench.autosave(None, cx);
                workbench.save_session(cx);
                workbench.for_each_intelligence(cx, |intelligence, _| intelligence.window_deactivated());
            }
            // A balloon does not fade while nobody can see it.
            workbench.balloon_view.window_active = window.is_window_active();
            workbench.sync_notifications(cx);
            // A conversation on screen counts as read only while the window is active.
            workbench.agents_window_active(window.is_window_active(), cx);
            // The debugger asks for attention on a stop only while the window is in the background.
            if let Some(driver) = workbench.debug_driver().cloned() {
                let active = window.is_window_active();
                driver.update(cx, |driver, cx| driver.set_window_focused(active, cx));
            }
            // Another window may have opened or forgotten a project meanwhile.
            if window.is_window_active() {
                workbench.sync_welcome(window, cx);
            }
        })
        .detach();
        let id = window.window_handle().window_id();
        let this = cx.weak_entity();
        cx.default_global::<Workbenches>().0.insert(id, this);
        // Events for this window, and those for every window, reach its models.
        let weak = cx.weak_entity();
        let listener = cx.try_global::<Backend>().is_some().then(|| {
            cx.global_mut::<Backend>().listen(move |event, cx| {
                let _ = weak.update(cx, |workbench, cx| workbench.backend_event(event, cx));
            })
        });
        cx.on_release(move |workbench, cx| {
            // The window is going: the session is written as it stands.
            workbench.save_session(cx);
            // Its project closes with it, and the project's own plugins unless another window has it.
            let root = cx.try_global::<Backend>().and_then(|backend| {
                let root = backend.host.project(&workbench.label).ok().map(|project| project.root.clone());
                backend.host.close_project_detached(&workbench.label);
                root
            });
            if let Some(root) = root.filter(|_| cx.has_global::<crate::plugins::Plugins>()) {
                crate::plugins::Plugins::release_project(root, cx);
            }
            if cx.has_global::<Backend>() {
                cx.global_mut::<Backend>().unlisten(workbench.listener);
            }
            workbench.stop_listening_to_plugins(cx);
            cx.default_global::<Workbenches>().0.remove(&id);
            workbench.stop_plugin_runtime(cx);
            input::window_blurred(id, cx);
            crate::platform::context::drop_window_context(id, cx);
        })
        .detach();
        let balloons = cx.new(|_| NotificationBalloons::new());
        let balloon_events = cx.subscribe_in(&balloons, window, |workbench, _, event: &BalloonEvent, window, cx| {
            match event.clone() {
                BalloonEvent::Dismissed(id) => workbench.notifications.dismiss_balloon(id),
                BalloonEvent::ActionChosen { id, action } => {
                    workbench.notifications.dismiss_balloon(id);
                    workbench.run_notification_action(action, window, cx);
                }
                BalloonEvent::HoverChanged(hovered) => workbench.balloon_view.hovered = hovered,
            }
            workbench.sync_notifications(cx);
        });
        let code_insight = code_insight::CodeInsight::new(label, window, cx);
        let chrome = chrome::Chrome::new(window, cx);
        let mut workbench = Self {
            label: label.to_owned(),
            content: Content::Empty,
            editor_observer: None,
            shown_title: None,
            tasks: Tasks::default(),
            notifications: Notifications::default(),
            task_timer: None,
            listener: listener.unwrap_or_default(),
            previous_editor: None,
            close_requests: Default::default(),
            leave_decided: false,
            app_dialog: None,
            session_save: None,
            popup: None,
            light_find_bar: None,
            editor_core: Default::default(),
            services: None,
            github: None,
            diff_window: None,
            run_widget: None,
            edit_configs: None,
            settings: None,
            plugin_ui: Default::default(),
            vcs: None,
            commit: None,
            git_log: None,
            vcs_dialog: None,
            language: Default::default(),
            extras: Default::default(),
            conflicts: Default::default(),
            vcs_operations: Default::default(),
            shelf: Default::default(),
            log_actions: Default::default(),
            structure: None,
            todo: None,
            build: None,
            files: Default::default(),
            database: None,
            code_insight,
            agents: None,
            debug: None,
            chrome,
            activations: HashMap::new(),
            active_key: None,
            bookmarks: BookmarkStore::default(),
            balloons,
            balloon_clocks: BalloonClocks::default(),
            balloon_view: BalloonView::default(),
            balloon_timer: None,
            _balloon_events: balloon_events,
            welcome: None,
            plugin_runtime: Default::default(),
            plugin_contributions: Default::default(),
            plugin_tool_windows: Default::default(),
        };
        workbench.start_plugin_runtime(window, cx);
        workbench.listen_to_plugins(window, cx);
        workbench.sync_welcome(window, cx);
        workbench.start_code_insight(cx);
        workbench
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// How much of the IDE runs for this window: Blaze (editor, git and search only) or Deep
    /// (language servers and the tooling feeding them too).
    pub fn ide_mode(&self, cx: &App) -> ide_core::settings::IdeMode {
        crate::platform::settings::effective(&self.label, cx).ide.mode
    }

    /// Switches between Blaze and Deep mode, for every project; a project whose own settings
    /// name the mode is switched too, so the toggle always takes effect in this window.
    pub fn toggle_ide_mode(&mut self, cx: &mut Context<Self>) {
        use crate::platform::settings::set;
        use ide_core::settings::IdeMode;
        use ide_workspace::settings::SettingsScope;
        let next = if self.ide_mode(cx).is_full() { IdeMode::Light } else { IdeMode::Full };
        let value = serde_json::Value::from(next.as_str());
        let label = self.label.clone();
        let in_project = matches!(self.content, Content::Project { .. });
        let switched = set(&label, SettingsScope::Global, "ide.mode", &value, cx).and_then(|settings| {
            if in_project && settings.ide.mode != next {
                set(&label, SettingsScope::Project, "ide.mode", &value, cx).map(|_| ())
            } else {
                Ok(())
            }
        });
        if let Err(error) = switched {
            let mut request = NotificationRequest::new(NotificationLevel::Error, "Cannot change the IDE mode");
            request.body = Some(error.message);
            self.notify(request, cx);
        }
        cx.notify();
    }

    /// The folder of the project open in this window.
    pub fn project_root(&self, cx: &App) -> Option<PathBuf> {
        match &self.content {
            Content::Project { tree, .. } => Some(PathBuf::from(tree.read(cx).root())),
            _ => None,
        }
    }

    /// Shows `request` in this window, subject to the notification settings.
    pub fn notify(&mut self, request: NotificationRequest, cx: &mut Context<Self>) {
        let settings = self.notification_settings(cx);
        let _ = self.notifications.notify(request, settings.as_ref(), now_ms());
        self.sync_notifications(cx);
    }

    /// Settings › Notifications as they are in effect for the window.
    fn notification_settings(&self, cx: &App) -> Option<NotificationSettings> {
        let settings = serde_json::to_value(crate::platform::settings::effective(&self.label, cx)).ok()?;
        notification_settings(&settings)
    }

    /// Brings the balloons and the Notifications tool window in line with the notifications:
    /// after one was raised, dismissed or faded, and when the tool window shows or hides.
    fn sync_notifications(&mut self, cx: &mut Context<Self>) {
        let settings = self.notification_settings(cx);
        let list_open = self.is_tool_window_visible(NOTIFICATIONS_TOOL_WINDOW);
        // What is listed in front of the user has been seen.
        if list_open && self.notifications.has_unseen() {
            self.notifications.mark_all_read();
        }
        self.balloon_view.list_open = list_open;
        self.balloon_clocks.sync(&self.notifications, self.balloon_view, now_ms());
        let shown = balloon_entries(&self.notifications, settings.as_ref(), list_open);
        self.balloons.update(cx, |balloons, cx| balloons.set(shown, cx));
        let list =
            self.tool_window_view(NOTIFICATIONS_TOOL_WINDOW).and_then(|view| view.downcast::<NotificationsView>().ok());
        if let Some(list) = list {
            let logged = logged_entries(&self.notifications, settings.as_ref());
            list.update(cx, |list, cx| list.set(logged, cx));
        }
        self.balloon_timer = self.balloon_clocks.next_deadline().map(|due| {
            let wait = std::time::Duration::from_secs_f64(((due - now_ms()) / 1000.0).max(0.0));
            cx.spawn(async move |workbench, cx| {
                cx.background_executor().timer(wait).await;
                let _ = workbench.update(cx, |workbench, cx| {
                    let view = workbench.balloon_view;
                    workbench.balloon_clocks.fade_out(&mut workbench.notifications, view, now_ms());
                    workbench.sync_notifications(cx);
                });
            })
        });
        cx.notify();
    }

    /// Does what a notification's link says: "Don't Show Again" turns its group's balloons off,
    /// a link of the notification's own runs its command when that is a registered action.
    fn run_notification_action(&mut self, action: EntryAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            EntryAction::DontShowAgain { group } => {
                let write = suppress_group(self.notification_settings(cx).as_ref(), &group);
                if let Err(error) = crate::platform::settings::set_global(&self.label, &[write], cx) {
                    tracing::warn!("cannot store the notification setting: {}", error.message);
                }
                self.notifications.hide_group_balloons(&group);
            }
            EntryAction::Own(own)
                if own.command.starts_with("run.showSession:") || own.command.starts_with("run.rerunSession:") =>
            {
                let Some(driver) = self.run_driver().cloned() else { return };
                let (command, session) = own.command.split_once(':').unwrap_or_default();
                let session = session.to_owned();
                if command == "run.rerunSession" {
                    driver.update(cx, |driver, cx| {
                        if let Some(rerun) = driver.rerun(&session, cx) {
                            rerun.detach();
                        }
                    });
                } else {
                    driver.update(cx, |driver, cx| {
                        driver.store().update(cx, |store, cx| {
                            store.active_session = Some(session.clone());
                            cx.notify();
                        });
                    });
                    self.with_layout(window, cx, |layout| layout.show(RUN_TOOL_WINDOW, true));
                }
            }
            EntryAction::Own(own) if own.command.starts_with(log_actions::UNDO_PREFIX) => {
                if !self.run_log_undo(&own.command, window, cx) {
                    tracing::debug!("the notification command {} has no handler any more", own.command);
                }
            }
            EntryAction::Own(own) if own.command.starts_with(conflicts::FOLLOW_UP_PREFIX) => {
                if !self.run_conflicts_follow_up(&own.command, window, cx) {
                    tracing::debug!("the notification command {} has no handler any more", own.command);
                }
            }
            // An agent task's notification: its button opens the task it names.
            // A plugin's notification: its button runs the publishing plugin's command.
            EntryAction::Own(own)
                if own.command.starts_with(crate::plugin_models::runtime_ui::NOTIFICATION_COMMAND_PREFIX) =>
            {
                self.run_plugin_notification_button(&own.command, cx);
            }
            EntryAction::Own(own) if own.command.starts_with("agents.openTask:") => {
                if let Some((_, task)) = own.command.split_once(':') {
                    self.open_agent_task(task, cx);
                }
            }
            EntryAction::Own(own) if own.command == editor_language::WORKSPACE_UNDO_COMMAND => {
                self.run_language_notification(&own.command, cx);
            }
            EntryAction::Own(own) if own.command.starts_with("vcs.deleted.") => {
                if !self.run_vcs_follow_up(&own.command, window, cx) {
                    tracing::debug!("the notification command {} has no handler any more", own.command);
                }
            }
            EntryAction::Own(own) => {
                if self.debug_notification_command(&own.command, cx) {
                    return;
                }
                let registered =
                    cx.try_global::<ActionRegistry>().is_some_and(|registry| registry.contains(&own.command));
                if registered {
                    window.defer(cx, move |window, cx| {
                        execute_action_from(ActionSource::Other, &own.command, None, window, cx);
                    });
                } else {
                    // Commands with arguments of their own (a run configuration, a repository) have
                    // no runner yet.
                    tracing::debug!("the notification command {} has no handler", own.command);
                }
            }
        }
    }

    /// Runs `update` on the code intelligence of every editor of the window.
    fn for_each_intelligence(
        &self,
        cx: &mut App,
        mut update: impl FnMut(&mut EditorIntelligence, &mut Context<EditorIntelligence>),
    ) {
        let editors: Vec<Entity<Editor>> = match &self.content {
            Content::Editor(editor) => vec![editor.clone()],
            Content::Project { editors, .. } => editors.values().cloned().collect(),
            _ => Vec::new(),
        };
        for editor in editors {
            if let Some(intelligence) = EditorIntelligence::of(&editor, cx) {
                intelligence.update(cx, &mut update);
            }
        }
    }

    pub fn editor(&self) -> Option<&Entity<Editor>> {
        match &self.content {
            Content::Editor(editor) => Some(editor),
            Content::Project { tabs, editors, .. } => {
                tabs.pane(&tabs.active_pane).and_then(|pane| pane.active.as_ref()).and_then(|id| editors.get(id))
            }
            _ => None,
        }
    }

    /// The editor tabs, when a project is open.
    pub fn tabs(&self) -> Option<&TabsState> {
        match &self.content {
            Content::Project { tabs, .. } => Some(tabs.as_ref()),
            _ => None,
        }
    }

    /// The titles of the active pane's tabs, in strip order. Files of the same name carry the
    /// folder that tells them apart as a hint.
    pub fn tab_titles(&self) -> Vec<TabTitle> {
        self.tabs().map(|tabs| self.tab_titles_of(&tabs.active_pane)).unwrap_or_default()
    }

    /// The titles of the tabs of `pane`, in strip order.
    pub fn tab_titles_of(&self, pane: &str) -> Vec<TabTitle> {
        let Some(pane) = self.tabs().and_then(|tabs| tabs.pane(pane)) else { return Vec::new() };
        let paths: Vec<&str> = pane.tabs.iter().filter_map(|tab| tab.input.path.as_deref()).collect();
        let hints = disambiguate_titles(&paths);
        pane.tabs
            .iter()
            .map(|tab| {
                let path = tab.input.path.as_deref().unwrap_or(&tab.input.key);
                let name = self
                    .tab_view(&tab.id)
                    .map(|view| view.title.clone())
                    .or_else(|| tab_views::title_of(&tab.input))
                    .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_owned());
                TabTitle {
                    name,
                    hint: hints.get(path).cloned(),
                    active: pane.active.as_deref() == Some(tab.id.as_str()),
                }
            })
            .collect()
    }

    /// One pane of the editor area: its tabs over the editor of the selected one.
    fn pane_element(&self, pane_id: &str, window: &Window, cx: &mut Context<Self>) -> gpui_kit::Div {
        let shown_tab = self.tabs().and_then(|tabs| tabs.pane(pane_id)).and_then(|pane| pane.active.clone());
        let content = shown_tab.as_deref().and_then(|id| self.tab_content(id));
        let editor = match content {
            Some(tab_views::TabContent::Editor(editor)) => Some(editor.clone()),
            _ => None,
        };
        let view = match content {
            Some(tab_views::TabContent::View(view)) => Some((view.view.clone(), view.focus.clone())),
            _ => None,
        };
        // A Markdown file's editor is drawn inside its layouts; the Find bar and the conflict
        // banner only go over a text editor.
        let host = shown_tab.as_deref().and_then(|id| self.markdown_host(id)).filter(|_| editor.is_some());
        let find_bar = match &self.content {
            Content::Project { find_bars, .. } if editor.is_some() => {
                shown_tab.as_deref().and_then(|id| find_bars.get(id)).map(|bar| bar.bar.clone())
            }
            _ => None,
        };
        // The banner of a file with unresolved merge conflicts sits above its editor.
        let banner =
            editor.as_ref().and_then(|editor| self.conflict_banner_for(&editor.read(cx).path().to_string_lossy()));
        let editor_focused = editor.as_ref().is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
            || view.as_ref().is_some_and(|(_, focus)| focus.contains_focused(window, cx));
        let layout_actions = host.as_ref().map(|host| host.update(cx, |host, cx| host.layout_actions(cx)));
        // A right click in the editor opens its menu (left of the text: the gutter's).
        let menu_editor = editor.clone();
        let body: Option<gpui_kit::AnyElement> = match (view, host, editor) {
            (Some((view, _)), _, _) => Some(view.into_any_element()),
            (None, Some(host), _) => Some(host.into_any_element()),
            (None, None, editor) => editor.map(IntoElement::into_any_element),
        };
        let titles = self.tab_titles_of(pane_id);
        let ids: Vec<(String, String)> = self
            .tabs()
            .and_then(|tabs| tabs.pane(pane_id))
            .map(|pane| {
                pane.tabs
                    .iter()
                    .map(|tab| {
                        let icon = match self.tab_view(&tab.id) {
                            Some(view) => view.icon.clone(),
                            None => tab_views::icon_of(&tab.input),
                        };
                        (tab.id.clone(), icon)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The tab strip: the selected tab is a rounded pill; the rest are plain titles. Distraction
        // Free Mode hides it.
        let strip =
            (!titles.is_empty() && !distraction_free_active(cx)).then(|| {
                div()
                    .h(theme::length(cx, "h-header"))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(gpui_kit::px(2.0))
                    .px(gpui_kit::px(4.0))
                    .border_b_1()
                    .border_color(theme::color(cx, "border-editor-tabs"))
                    .children(titles.into_iter().zip(ids).map(|(TabTitle { name, hint, active }, (id, icon))| {
                        let (select, close) = (id.clone(), id.clone());
                        let (select_pane, close_pane) = (pane_id.to_owned(), pane_id.to_owned());
                        let (menu_pane, menu_tab) = (pane_id.to_owned(), id.clone());
                        let selected = if editor_focused { "bg-tab-selected" } else { "bg-tab-selected-inactive" };
                        div()
                            .id(SharedString::from(format!("tab:{id}")))
                            .debug_selector(|| format!("tab:{id}"))
                            .h(theme::length(cx, "h-tab"))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(gpui_kit::px(6.0))
                            .pl(gpui_kit::px(8.0))
                            .pr(gpui_kit::px(4.0))
                            .rounded(gpui_kit::px(6.0))
                            .text_color(theme::color(cx, "fg-strong"))
                            .when(active, |el| el.bg(theme::color(cx, selected)))
                            .when(!active, |el| el.hover(|el| el.bg(theme::color(cx, "bg-tab-hover"))))
                            .child(theme::icon(cx, &icon, 16.0, theme::color(cx, "fg-strong")))
                            .child(SharedString::from(name))
                            .children(hint.map(|hint| {
                                div().text_color(theme::color(cx, "fg-muted")).child(SharedString::from(hint))
                            }))
                            .child(
                                div()
                                    .id(SharedString::from(format!("close:{id}")))
                                    .debug_selector(|| format!("close:{id}"))
                                    .with_tooltip("Close Tab")
                                    .size(gpui_kit::px(16.0))
                                    .flex_none()
                                    .rounded(gpui_kit::px(4.0))
                                    .hover(|el| el.bg(theme::color(cx, "bg-tab-hover")))
                                    .child(theme::icon(cx, "close", 16.0, theme::color(cx, "fg-muted")))
                                    .on_click(cx.listener(move |workbench, _, window, cx| {
                                        workbench.with_tabs(window, cx, |tabs, _| tabs.close_tab(&close_pane, &close));
                                        cx.stop_propagation();
                                    })),
                            )
                            .on_click(cx.listener(move |workbench, _, window, cx| {
                                workbench.with_tabs(window, cx, |tabs, _| {
                                    tabs.set_active_pane(&select_pane);
                                    tabs.activate_tab(&select_pane, &select);
                                    true
                                });
                            }))
                            .on_mouse_down(
                                gpui_kit::MouseButton::Right,
                                cx.listener(move |workbench, event: &MouseDownEvent, window, cx| {
                                    let items = workbench.tab_menu(&menu_pane, &menu_tab, window, cx);
                                    let (pane, tab) = (menu_pane.clone(), menu_tab.clone());
                                    workbench.show_menu(
                                        &items,
                                        event.position,
                                        move |workbench, command, window, cx| {
                                            workbench.run_tab_command(&pane, &tab, command, window, cx);
                                        },
                                        window,
                                        cx,
                                    );
                                }),
                            )
                    }))
                    .child(div().flex_1())
                    .children(context_menus::slot_buttons("editor.title", window, cx))
                    .children(layout_actions)
            });
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .children(strip)
            .children(banner.map(|banner| div().flex_none().child(banner)))
            .children(find_bar.map(|bar| div().flex_none().child(bar)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when_some(menu_editor, |el, editor| {
                        el.on_mouse_down(
                            gpui_kit::MouseButton::Right,
                            cx.listener(move |workbench, event: &MouseDownEvent, window, cx| {
                                workbench.open_editor_context_menu(&editor, event.position, window, cx);
                                cx.stop_propagation();
                            }),
                        )
                    })
                    .children(body),
            )
    }

    /// The editor area: the panes as the splits arrange them, side by side or stacked, sharing
    /// the room evenly with a line between them.
    fn editor_area(&self, node: &LayoutNode, window: &Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        match node {
            LayoutNode::Pane { pane_id } => self.pane_element(pane_id, window, cx).into_any_element(),
            LayoutNode::Split { dir, children, .. } => {
                let stacked = *dir == SplitDir::Column;
                let line = theme::color(cx, "border-editor-tabs");
                let parts: Vec<gpui_kit::AnyElement> = children
                    .iter()
                    .enumerate()
                    .map(|(index, child)| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .when(index > 0 && stacked, |el| el.border_t_1().border_color(line))
                            .when(index > 0 && !stacked, |el| el.border_l_1().border_color(line))
                            .child(self.editor_area(child, window, cx))
                            .into_any_element()
                    })
                    .collect();
                div().size_full().flex().when(stacked, |el| el.flex_col()).children(parts).into_any_element()
            }
        }
    }

    /// Changes the editor tabs with `change` (given the model and the active pane's id), then
    /// brings the window in line: editors of closed tabs are saved and released, and the focus
    /// goes to the tab that is now active.
    pub fn with_tabs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut TabsState, &str) -> bool,
    ) {
        let Content::Project { tabs, .. } = &mut self.content else { return };
        let pane = tabs.active_pane.clone();
        if change(tabs, &pane) {
            self.tabs_changed(true, window, cx);
        }
    }

    fn tabs_changed(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let auto_save = self.auto_save(cx);
        let Content::Project { tabs, editors, editor_focus, find_bars, .. } = &mut self.content else { return };
        let open: std::collections::HashSet<&str> =
            tabs.panes.iter().flat_map(|pane| pane.tabs.iter().map(|tab| tab.id.as_str())).collect();
        find_bars.retain(|id, _| open.contains(id.as_str()));
        let closed: Vec<String> = editors.keys().filter(|id| !open.contains(id.as_str())).cloned().collect();
        // The unsaved editors of closed tabs: the close writes them, so leaving them does not.
        let mut closing = Vec::new();
        for id in closed {
            editor_focus.remove(&id);
            let Some(editor) = editors.remove(&id) else { continue };
            // Closing never loses edits: an unsaved document is written before it is released,
            // and when it cannot be written its tab comes back with the text still in it. With
            // autosave off the user decides instead.
            if !editor.read(cx).is_dirty() {
                continue;
            }
            closing.push(editor.clone());
            let decision = (!auto_save).then(|| {
                let path = editor.read(cx).path().to_string_lossy().into_owned();
                let i18n = &cx.default_global::<crate::platform::i18n::Localization>().0;
                let (title, body) = (
                    i18n.t("app.unsaved.title", &[]),
                    i18n.t("app.unsaved.body", &[("count", 1usize.into()), ("project", path.into())]),
                );
                let answers =
                    [i18n.t("app.unsaved.save", &[]), i18n.t("app.unsaved.discard", &[]), i18n.t("common.cancel", &[])];
                let answers: Vec<&str> = answers.iter().map(String::as_str).collect();
                window.prompt(gpui_kit::PromptLevel::Warning, &title, Some(&body), &answers, cx)
            });
            cx.spawn_in(window, async move |workbench, cx| {
                let answer = match decision {
                    Some(decision) => decision.await.ok(),
                    None => Some(0),
                };
                let failure = match answer {
                    Some(0) => {
                        let saving = editor.read_with(cx, |editor, cx| editor.save(cx));
                        match saving.await {
                            Ok(Ok(())) => return,
                            Ok(Err(error)) => Some(error.message),
                            Err(error) => Some(error.to_string()),
                        }
                    }
                    // Don't Save.
                    Some(1) => return,
                    // Cancel, or the question went away unanswered: the tab stays.
                    _ => None,
                };
                let _ = workbench
                    .update_in(cx, |workbench, window, cx| workbench.restore_unsaved(editor, failure, window, cx));
            })
            .detach();
        }
        self.release_closed_views(window, cx);
        self.load_active_tab(focus, window, cx);
        self.note_active_tab(cx);
        self.plugin_tabs_changed(cx);
        self.session_save = Some(cx.spawn(async move |workbench, cx| {
            cx.background_executor().timer(SESSION_SAVE_DELAY).await;
            let _ = workbench.update(cx, |workbench, cx| workbench.save_session(cx));
        }));
        let active = self.editor().cloned();
        // A view tab takes the keyboard itself; a Markdown file in Preview Only gives it to the preview.
        let target = self.active_focus_target(cx);
        // The editor that was active is saved when another one takes its place.
        let previous = self.previous_editor.take().and_then(|previous| previous.upgrade());
        if let Some(previous) =
            previous.filter(|previous| active.as_ref() != Some(previous) && !closing.contains(previous))
        {
            self.autosave(Some(&previous), cx);
        }
        self.previous_editor = active.as_ref().map(Entity::downgrade);
        match active {
            Some(editor) => {
                self.observe_editor(&editor, cx);
                if focus {
                    let handle = target.unwrap_or_else(|| editor.focus_handle(cx));
                    window.focus(&handle, cx);
                }
            }
            None => {
                self.editor_observer = None;
                self.sync_structure_window(cx);
                self.sync_todo_window(cx);
                self.sync_code_insight(cx);
                if focus && let Some(handle) = target {
                    window.focus(&handle, cx);
                }
            }
        }
        cx.notify();
    }

    /// A closed tab whose document was not saved, because saving failed or the user chose to
    /// stay: it reopens with its editor, and a failure is reported.
    fn restore_unsaved(
        &mut self,
        editor: Entity<Editor>,
        failure: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = editor.read(cx).path().to_string_lossy().into_owned();
        if let Some(failure) = failure {
            self.report_save_failure(failure, cx);
        }
        let Content::Project { tabs, editors, find_bars, view_tabs, .. } = &mut self.content else { return };
        let pane = tabs.active_pane.clone();
        let input = EditorInput { kind: "file".to_owned(), key: path.clone(), path: Some(path), data: None };
        let reopened = tabs.open_tab(&pane, input, OpenTabOptions::default());
        if let Some(id) = &reopened {
            // A tab for the same file opened meanwhile has its own editor: the unsaved one wins,
            // since it is the one holding the text that is not on disk. A bar and layouts around
            // the editor it replaces go with that editor.
            find_bars.remove(id);
            view_tabs.drop_tab(id);
            editors.insert(id.clone(), editor.clone());
        }
        if let Some(id) = reopened {
            self.wrap_markdown(&id, &editor, window, cx);
        }
        self.tabs_changed(true, window, cx);
    }

    fn report_save_failure(&mut self, failure: String, cx: &mut Context<Self>) {
        let title = cx.default_global::<crate::platform::i18n::Localization>().0.t("app.unsaved.saveFailed", &[]);
        let mut request = NotificationRequest::new(NotificationLevel::Error, title);
        request.body = Some(failure);
        self.notify(request, cx);
    }

    /// Every open editor, of every tab.
    fn editors(&self) -> Vec<Entity<Editor>> {
        match &self.content {
            Content::Editor(editor) => vec![editor.clone()],
            Content::Project { editors, .. } => editors.values().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// How many open documents have changes that are not on disk.
    pub fn unsaved_count(&self, cx: &App) -> usize {
        self.editors().iter().filter(|editor| editor.read(cx).is_dirty()).count()
    }

    /// Files › Autosave (`files.autoSave`): save when a tab is left or closed and when the
    /// window loses focus.
    fn auto_save(&self, cx: &App) -> bool {
        crate::platform::settings::effective(&self.label, cx).files.auto_save
    }

    /// With autosave on, saves `editor` if it is unsaved, or every unsaved editor when none is
    /// named. A failure is reported; the text stays in its editor.
    fn autosave(&mut self, editor: Option<&Entity<Editor>>, cx: &mut Context<Self>) {
        // While a close is decided its question owns the unsaved documents: Don't Save must hold.
        if !self.auto_save(cx) || self.close_requests.deciding() {
            return;
        }
        let editors = match editor {
            Some(editor) => vec![editor.clone()],
            None => self.editors(),
        };
        for editor in editors.into_iter().filter(|editor| editor.read(cx).is_dirty()) {
            let saving = editor.read(cx).save(cx);
            cx.spawn(async move |workbench, cx| {
                let failure = match saving.await {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => error.message,
                    Err(error) => error.to_string(),
                };
                let _ = workbench.update(cx, |workbench, cx| workbench.report_save_failure(failure, cx));
            })
            .detach();
        }
    }

    /// Saves every unsaved editor; the first failure's message, if any.
    fn save_all(&self, cx: &App) -> impl Future<Output = Result<(), String>> + 'static + use<> {
        let saving: Vec<_> = self
            .editors()
            .iter()
            .filter(|editor| editor.read(cx).is_dirty())
            .map(|editor| editor.read(cx).save(cx))
            .collect();
        async move {
            let mut failure = None;
            for save in saving {
                match save.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => failure = failure.or(Some(error.message)),
                    Err(error) => failure = failure.or(Some(error.to_string())),
                }
            }
            failure.map_or(Ok(()), Err)
        }
    }

    /// Background tasks of this window, in listing order.
    pub fn tasks(&self) -> &Tasks {
        &self.tasks
    }

    pub fn notifications(&self) -> &Notifications {
        &self.notifications
    }

    /// What the status bar says about background work: the first task's title with its message,
    /// and how many more are running.
    pub fn task_status(&self) -> Option<String> {
        let tasks = self.tasks.tasks();
        let first = tasks.first()?;
        let mut text = match &first.message {
            Some(message) if !message.is_empty() => format!("{}: {message}", first.title),
            _ => first.title.clone(),
        };
        if tasks.len() > 1 {
            text.push_str(&format!(" (+{})", tasks.len() - 1));
        }
        Some(text)
    }

    /// A backend event: its task, indexing and notification reports feed this window's models.
    pub fn backend_event(&mut self, event: &BackendEvent, cx: &mut Context<Self>) {
        if event.window.as_deref().is_some_and(|window| window != self.label) {
            return;
        }
        let now = now_ms();
        match &event.event {
            IdeEvent::Task { id, title, message, fraction, done, cancellable } => self.tasks.on_task_event(
                TaskEvent {
                    id: u64::from(id.0),
                    title: title.clone(),
                    message: message.clone(),
                    fraction: fraction.map(f64::from),
                    done: *done,
                    cancellable: *cancellable,
                },
                now,
            ),
            IdeEvent::IndexState { indexing, files, symbols } => {
                self.tasks.set_indexing(*indexing, u64::from(*files), u64::from(*symbols), now);
            }
            IdeEvent::VcsChanged { .. } | IdeEvent::FsChanged { .. } => {
                self.vcs_backend_event(&event.event, cx);
                return;
            }
            IdeEvent::Debug { .. } => {
                self.debug_backend_event(&event.event, cx);
                return;
            }
            IdeEvent::RunExited { .. } => {
                if let Some(driver) = self.run_driver().cloned() {
                    driver.update(cx, |driver, cx| driver.backend_event(&event.event, cx));
                }
                return;
            }
            // The keymap and the open Settings dialog follow; Settings › Console reaches the
            // consoles and the Run tool window.
            IdeEvent::SettingsChanged { key } => {
                self.settings_changed(key, cx);
                if let Some(driver) = self.run_driver().cloned() {
                    driver.update(cx, |driver, cx| driver.apply_console_settings(cx));
                    self.sync_run_window(cx);
                }
                self.code_insight_settings_changed(cx);
                return;
            }
            IdeEvent::Notify { level, title, body } => {
                let level = match level {
                    NotifyLevel::Info => NotificationLevel::Info,
                    NotifyLevel::Warning => NotificationLevel::Warning,
                    NotifyLevel::Error => NotificationLevel::Error,
                };
                let mut request = NotificationRequest::new(level, title.clone());
                request.body = body.clone();
                let settings = self.notification_settings(cx);
                let _ = self.notifications.notify(request, settings.as_ref(), now);
                self.sync_notifications(cx);
            }
            IdeEvent::Diagnostics { path, source, items } => {
                self.diagnostics_published(path, source, items, cx);
                return;
            }
            IdeEvent::LanguageServer { server, name, status, message } => {
                self.language_server_event(*server, name, *status, message.clone(), cx);
                return;
            }
            IdeEvent::LanguageServerRemoved { server } => {
                self.language_server_removed(*server, cx);
                return;
            }
            IdeEvent::DbSourcesChanged => {
                self.database_sources_changed(cx);
                return;
            }
            // The project's agents are listed again, with or without the agents themselves.
            IdeEvent::ProjectTrustChanged { trusted, .. } => {
                self.agents_trust_changed(*trusted, cx);
                // The Restricted Mode banner comes or goes.
                cx.notify();
                return;
            }
            _ => return,
        }
        self.schedule_tasks(cx);
        cx.notify();
    }

    /// Advances the task list to `now` and arranges to be woken at its next deadline.
    pub fn tick_tasks(&mut self, now: f64, cx: &mut Context<Self>) {
        self.tasks.expire(now);
        self.schedule_tasks(cx);
        cx.notify();
    }

    fn schedule_tasks(&mut self, cx: &mut Context<Self>) {
        self.task_timer = self.tasks.next_deadline().map(|due| {
            let wait = std::time::Duration::from_secs_f64(((due - now_ms()) / 1000.0).max(0.0));
            cx.spawn(async move |workbench, cx| {
                cx.background_executor().timer(wait).await;
                let _ = workbench.update(cx, |workbench, cx| workbench.tick_tasks(now_ms(), cx));
            })
        });
    }

    /// The status bar's right-hand items for the shown editor: caret position, line separator
    /// and encoding, in that order.
    pub fn status_items(&self, cx: &App) -> Vec<String> {
        let Some(status) = self.editor().map(|editor| editor.read(cx).status()) else { return Vec::new() };
        let encoding = status.encoding.to_lowercase();
        let shown = ENCODINGS.iter().find(|(id, _)| *id == encoding).map_or(status.encoding.as_str(), |(_, name)| name);
        vec![status.caret_text(), status.line_ending_text().to_owned(), shown.to_owned()]
    }

    /// The Line Separator item's menu: the three separators, the file's own marked.
    pub fn line_ending_menu(&self, cx: &App) -> Vec<MenuItem<StatusCommand>> {
        use ide_text::LineEnding;
        let Some(status) = self.editor().map(|editor| editor.read(cx).status()) else { return Vec::new() };
        [
            (LineEnding::Lf, "LF – Unix and macOS (\\n)"),
            (LineEnding::CrLf, "CRLF – Windows (\\r\\n)"),
            (LineEnding::Cr, "CR – Classic Mac OS (\\r)"),
        ]
        .into_iter()
        .map(|(line_ending, label)| {
            let row = MenuItem::item(label, StatusCommand::LineEnding(line_ending));
            if line_ending == status.line_ending { row.icon("check") } else { row }
        })
        .collect()
    }

    /// The File Encoding item's menu: reload the file in another encoding, or convert it to one.
    pub fn encoding_menu(&self, cx: &App) -> Vec<MenuItem<StatusCommand>> {
        use crate::vcs_ops::menu::MenuEntry;
        let Some(editor) = self.editor().map(|editor| editor.read(cx)) else { return Vec::new() };
        let (status, dirty) = (editor.status(), editor.is_dirty());
        let current = status.encoding.to_lowercase();
        let others = |reload: bool| -> Vec<MenuItem<StatusCommand>> {
            ENCODINGS
                .iter()
                .filter(|(id, _)| *id != current)
                .map(|(id, name)| {
                    let command = if reload {
                        StatusCommand::Reload((*id).to_owned())
                    } else {
                        StatusCommand::Convert((*id).to_owned())
                    };
                    MenuItem::item(*name, command)
                })
                .collect()
        };
        let group = |label: &str, disabled: bool, submenu: Vec<MenuItem<StatusCommand>>| {
            MenuItem::Item(MenuEntry {
                label: label.to_owned(),
                icon: None,
                shortcut: None,
                disabled,
                danger: false,
                run: None,
                submenu,
            })
        };
        vec![
            // Reloading would lose what is not saved; a read-only file cannot be written in another encoding.
            group("Reload with Encoding", dirty, others(true)),
            group("Convert to Encoding", status.read_only, others(false)),
            MenuItem::Separator,
            MenuItem::action_with("file.settings", "editor.fileEncodings".into()).labelled("File Encodings Settings…"),
        ]
    }

    /// Carries out a row of a status bar item's menu on the file being edited; a refusal is told
    /// in a notification.
    pub fn run_status_command(&mut self, command: StatusCommand, cx: &mut Context<Self>) {
        let Some(editor) = self.editor().cloned() else { return };
        let (title, done) = match &command {
            StatusCommand::LineEnding(line_ending) => (
                "Cannot change line separators",
                editor.update(cx, |editor, cx| editor.set_line_ending(*line_ending, cx)),
            ),
            StatusCommand::Reload(encoding) => {
                ("Cannot reload with encoding", editor.update(cx, |editor, cx| editor.set_encoding(encoding, true, cx)))
            }
            StatusCommand::Convert(encoding) => {
                ("Cannot convert encoding", editor.update(cx, |editor, cx| editor.set_encoding(encoding, false, cx)))
            }
        };
        if let Err(error) = done {
            let request = NotificationRequest {
                body: Some(error.message),
                ..NotificationRequest::new(NotificationLevel::Error, title)
            };
            self.notify(request, cx);
        }
        cx.notify();
    }

    /// The window's title for what it shows now.
    pub fn title(&self, cx: &App) -> String {
        let project = match &self.content {
            Content::Project { name, .. } => Some(name.as_str()),
            _ => None,
        };
        let file = self
            .editor()
            .map(|editor| editor.read(cx).path().to_path_buf())
            .or_else(|| self.active_tab_view().and_then(|view| view.input.path.as_deref()).map(PathBuf::from));
        let default = cx
            .try_global::<crate::platform::i18n::Localization>()
            .map_or_else(|| "app.title.default".to_owned(), |l| l.0.t("app.title.default", &[]));
        window_title(project, file.as_deref(), false, None, &default)
    }

    /// Publishes the facts `when` clauses read about this window: what is open and where the
    /// focus is. Unchanged values cost nothing.
    fn publish_context(&mut self, window: &mut Window, cx: &mut App) {
        use crate::platform::context::{clear_context, set_context};
        let editor = self.editor().cloned();
        let focused = editor.as_ref().is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
        set_context(window, cx, "projectOpening", matches!(self.content, Content::Opening(_)));
        let root = cx.try_global::<Backend>().and_then(|backend| backend.host.project(&self.label).ok());
        match root.map(|project| project.root.to_string_lossy().into_owned()) {
            Some(root) => set_context(window, cx, crate::plugins::PROJECT_ROOT_KEY, root),
            None => clear_context(window, cx, crate::plugins::PROJECT_ROOT_KEY),
        }
        set_context(window, cx, "popupOpen", self.popup.is_some());
        self.publish_run_context(window, cx);
        self.publish_debug_context(window, cx);
        self.publish_vcs_context(window, cx);
        self.publish_github_context(window, cx);
        self.publish_agents_context(window, cx);
        self.publish_merge_context(window, cx);
        self.publish_app_context(window, cx);
        self.publish_shelf_context(window, cx);
        self.publish_log_context(window, cx);
        set_context(window, cx, "findPopupOpen", self.find_popup().is_some());
        let focused_bar = self.focused_find_bar(window, cx);
        set_context(window, cx, "findBarOpen", !self.shown_find_bars().is_empty());
        set_context(window, cx, "findBarFocus", focused_bar.is_some());
        set_context(window, cx, "findBarHasResults", focused_bar.is_some_and(|bar| bar.read(cx).has_results()));
        set_context(window, cx, "pasteHistoryOpen", self.paste_history_dialog().is_some());
        // The dialog's list and filter are one keyboard target: while it has the keyboard, the keys
        // bound to Paste and Delete in the keymap run them.
        let paste_list_focused =
            self.paste_history_dialog().is_some_and(|dialog| dialog.focus_handle(cx).contains_focused(window, cx));
        set_context(window, cx, "pasteHistoryListFocus", paste_list_focused);
        set_context(window, cx, "projectOpen", self.project_tree().is_some());
        set_context(window, cx, "lightEdit", matches!(self.content, Content::Editor(_) | Content::Viewer(_)));
        set_context(window, cx, "textEditorActive", editor.is_some());
        // `imageViewerActive` / `imageViewerFocus` for an image tab; every kind's keys are off on
        // other tabs.
        self.publish_view_context(window, cx);
        set_context(window, cx, "editorSplit", self.tabs().is_some_and(|tabs| tabs.panes.len() > 1));
        let tabs_open =
            self.tabs().and_then(|tabs| tabs.pane(&tabs.active_pane)).is_some_and(|pane| !pane.tabs.is_empty());
        set_context(window, cx, "editorTabsOpen", tabs_open);
        for key in ["editorFocus", "editorTextFocus", "editorAreaFocus"] {
            set_context(window, cx, key, focused);
        }
        let focused_panel = match &self.content {
            Content::Project { panels, .. } => panels
                .iter()
                .find(|(_, panel)| (panel.focus)(cx).contains_focused(window, cx))
                .map(|(id, _)| id.clone()),
            _ => None,
        };
        set_context(window, cx, "toolWindowFocus", focused_panel.is_some());
        set_context(window, cx, "projectViewFocus", focused_panel.as_deref() == Some(PROJECT_TOOL_WINDOW));
        // Only the focused Project view lends its selected folder to the path actions (Find in Path…).
        let focused_directory = (focused_panel.as_deref() == Some(PROJECT_TOOL_WINDOW))
            .then(|| self.project_tree().and_then(|tree| tree.read(cx).selected_directory()))
            .flatten();
        match focused_directory {
            Some(directory) => set_context(window, cx, "focusedDirectory", directory),
            None => crate::platform::context::clear_context(window, cx, "focusedDirectory"),
        }
        set_context(window, cx, "terminalFocus", focused_panel.as_deref() == Some(TERMINAL_TOOL_WINDOW));
        set_context(window, cx, "servicesViewFocus", focused_panel.as_deref() == Some(SERVICES_TOOL_WINDOW));
        let database_focused =
            self.database_tree().is_some_and(|tree| tree.focus_handle(cx).contains_focused(window, cx));
        set_context(window, cx, database::DATABASE_VIEW_FOCUS, database_focused);
        self.publish_diff_context(window, cx);
        if let Content::Project { layout, .. } = &mut self.content {
            // The layout decides by this what `toolwindow.<id>` does: focus, or hide.
            layout.set_focused(focused_panel.as_deref());
            for (key, visible) in layout.visibility_context() {
                set_context(window, cx, &key, visible);
            }
            match layout.focused().map(str::to_owned) {
                Some(id) => set_context(window, cx, "activeToolWindow", id),
                None => clear_context(window, cx, "activeToolWindow"),
            }
        }
        match editor.map(|editor| {
            let editor = editor.read(cx);
            (editor.path().to_string_lossy().into_owned(), editor.language().to_string(), editor.status())
        }) {
            Some((path, language, status)) => {
                set_context(window, cx, "editorPath", path);
                set_context(window, cx, "editorLangId", language);
                set_context(window, cx, "editorReadOnly", status.read_only);
                set_context(window, cx, "editorHasSelection", status.selected_chars > 0);
            }
            None => {
                clear_context(window, cx, "editorPath");
                clear_context(window, cx, "editorLangId");
                set_context(window, cx, "editorReadOnly", false);
                set_context(window, cx, "editorHasSelection", false);
            }
        }
    }

    fn observe_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        self.editor_observer = Some(cx.observe(editor, |_, _, cx| cx.notify()));
        self.sync_structure_window(cx);
        self.sync_todo_window(cx);
        self.sync_code_insight(cx);
    }

    /// The tool window layout, when a project is open.
    pub fn tool_windows(&self) -> Option<&ToolWindowLayout> {
        match &self.content {
            Content::Project { layout, .. } => Some(layout.as_ref()),
            _ => None,
        }
    }

    /// Changes the tool window layout with `change` and carries out what it asks for: storing
    /// the layout, moving the focus to the editor, or to the tool window that requested it.
    pub fn with_layout(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut ToolWindowLayout) -> Effects,
    ) {
        // The active tab takes the keyboard back: its text editor, or the view of a tab that has none.
        let editor_focus = self
            .editor()
            .map(|editor| editor.focus_handle(cx))
            .or_else(|| self.active_tab_view().map(|v| v.focus.clone()));
        let Content::Project { layout, .. } = &mut self.content else { return };
        let effects = change(layout);
        if effects.persist {
            match serde_json::to_string(&layout.serialized()) {
                Ok(json) => cx.default_global::<UiStorage>().set(&storage_key(layout.scope()), json),
                Err(error) => tracing::warn!("cannot store the tool window layout: {error}"),
            }
        }
        // A window shown for the first time gets its view before the focus can go to it.
        self.mount_panels(window, cx);
        let Content::Project { layout, focus_served, panels, .. } = &mut self.content else { return };
        let request = layout.focus_request().filter(|request| request.seq > *focus_served).cloned();
        if let Some(request) = request {
            *focus_served = request.seq;
            if let Some(panel) = panels.get(&request.id) {
                let focus = (panel.focus)(cx);
                window.focus(&focus, cx);
            }
        } else if effects.focus_editor {
            match editor_focus {
                Some(focus) => window.focus(&focus, cx),
                // Nothing to edit: the focus leaves the tool window all the same.
                None => window.blur(cx),
            }
        }
        // The Notifications window may have opened or closed, or just got its view.
        self.sync_notifications(cx);
        self.sync_run_window(cx);
    }

    /// Creates the view of every visible tool window that has none yet.
    fn mount_panels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Project { layout, panels, tree, .. } = &self.content else { return };
        let missing: Vec<String> =
            layout.visible().ids().filter(|id| !panels.contains_key(*id)).map(str::to_owned).collect();
        let root = PathBuf::from(tree.read(cx).root());
        for id in missing {
            let panel = match id.as_str() {
                TERMINAL_TOOL_WINDOW => {
                    use crate::terminal::panel::{TerminalPanel, TerminalPanelEvent};
                    let (label, root) = (self.label.clone(), root.clone());
                    let terminals = cx.new(|cx| TerminalPanel::new(&label, root, cx));
                    // The last tab closing leaves nothing to show.
                    let closes =
                        cx.subscribe_in(&terminals, window, |workbench, _, event: &TerminalPanelEvent, window, cx| {
                            let TerminalPanelEvent::Emptied = event;
                            workbench.with_layout(window, cx, |layout| layout.hide(TERMINAL_TOOL_WINDOW));
                        });
                    Panel::new(terminals, Some(closes))
                }
                GIT_TOOL_WINDOW => self.mount_git_log(window, cx),
                NOTIFICATIONS_TOOL_WINDOW => {
                    let list = cx.new(NotificationsView::new);
                    let events =
                        cx.subscribe_in(&list, window, |workbench, _, event: &NotificationsEvent, window, cx| {
                            match event.clone() {
                                NotificationsEvent::Removed(id) => workbench.notifications.remove_notification(id),
                                // An entry of the list stays after its link was used.
                                NotificationsEvent::ActionChosen { action, .. } => {
                                    workbench.run_notification_action(action, window, cx);
                                }
                                NotificationsEvent::ClearAll => workbench.notifications.clear(),
                            }
                            workbench.sync_notifications(cx);
                        });
                    Panel::new(list, Some(events))
                }
                COMMIT_TOOL_WINDOW => {
                    use crate::vcs::commit_view::{CommitPanel, CommitPanelEvent};
                    let (label, root) = (self.label.clone(), root.clone());
                    let repository = self.vcs_state().filter(|vcs| vcs.available);
                    let status = repository.map(|vcs| if vcs.status_loaded { vcs.status.clone() } else { Vec::new() });
                    let fresh = status.is_some().then(|| self.request_vcs_refresh(cx));
                    let changes = cx.new(|cx| CommitPanel::following_window(&label, root, status, cx));
                    if let Some(fresh) = fresh {
                        let panel = changes.downgrade();
                        cx.spawn(async move |_, cx| {
                            // Settled, or dropped with the project's repository: either way no
                            // read is coming, and the panel shows what it has.
                            let _ = fresh.await;
                            // The panel may have gone with its tool window.
                            let _ = panel.update(cx, |panel, cx| panel.window_read_shown(cx));
                        })
                        .detach();
                    }
                    let opens =
                        cx.subscribe_in(&changes, window, |workbench, _, event: &CommitPanelEvent, window, cx| {
                            workbench.commit_panel_event(event, window, cx);
                        });
                    self.attach_commit_box(&changes, window, cx);
                    self.attach_changes_extras(&changes, cx);
                    Panel::new(changes, Some(opens))
                }
                RUN_TOOL_WINDOW => {
                    let Content::Project { run, .. } = &self.content else { continue };
                    let (store, types) = (run.read(cx).store().clone(), run.read(cx).types());
                    let view = cx.new(|cx| RunView::new(store, types, cx));
                    let events = cx.subscribe_in(&view, window, Self::run_view_event);
                    Panel::new(view, Some(events))
                }
                BOOKMARKS_TOOL_WINDOW => self.mount_bookmarks(window, cx),
                STRUCTURE_TOOL_WINDOW => self.mount_structure(window, cx),
                TODO_TOOL_WINDOW => self.mount_todo(window, cx),
                PROBLEMS_TOOL_WINDOW => self.mount_problems(window, cx),
                USAGES_TOOL_WINDOW => self.mount_usages(window, cx),
                HIERARCHY_TOOL_WINDOW => self.mount_hierarchy(window, cx),
                BUILD_TOOL_WINDOW => match self.mount_build(window, cx) {
                    Some(panel) => panel,
                    None => continue,
                },
                DATABASE_TOOL_WINDOW => match self.mount_database(window, cx) {
                    Some(panel) => panel,
                    None => continue,
                },
                DEBUG_TOOL_WINDOW => match self.mount_debug_window(window, cx) {
                    Some(panel) => panel,
                    None => continue,
                },
                SERVICES_TOOL_WINDOW => match self.mount_services(window, cx) {
                    Some(panel) => panel,
                    None => continue,
                },
                PULL_REQUESTS_TOOL_WINDOW => match self.mount_pull_requests(window, cx) {
                    Some(panel) => panel,
                    None => continue,
                },
                crate::agents::model::TOOL_WINDOW => match self.mount_agents_panel(window, cx) {
                    Some(panel) => panel,
                    None => continue,
                },
                FIND_TOOL_WINDOW => {
                    use crate::find::view::{FindPanel, FindPanelEvent};
                    let label = self.label.clone();
                    let find = cx.new(|cx| FindPanel::new(&label, cx));
                    let opens =
                        cx.subscribe_in(
                            &find,
                            window,
                            |workbench, _, event: &FindPanelEvent, window, cx| match event {
                                FindPanelEvent::Open { path, line, col } => {
                                    workbench.open_project_file_at(path.clone(), *line, *col, window, cx);
                                }
                                FindPanelEvent::ContextMenu { path, line, col, position } => {
                                    workbench.show_search_result_menu(path.clone(), *line, *col, *position, window, cx);
                                }
                            },
                        );
                    Panel::new(find, Some(opens))
                }
                // A plugin's view, or nothing this app draws.
                other => match self.mount_plugin_tool_window(other, window, cx) {
                    Some(panel) => panel,
                    None => continue,
                },
            };
            if let Content::Project { panels, .. } = &mut self.content {
                panels.insert(id, panel);
            }
        }
        // The Terminal shown again after its last tab closed starts a new one.
        if self.is_tool_window_visible(TERMINAL_TOOL_WINDOW)
            && let Some(terminals) = self
                .tool_window_view(TERMINAL_TOOL_WINDOW)
                .and_then(|view| view.downcast::<crate::terminal::panel::TerminalPanel>().ok())
            && terminals.read(cx).terminals().tabs().is_empty()
        {
            terminals.update(cx, |terminals, cx| terminals.open_tab(cx));
        }
        self.sync_services_window(cx);
        self.sync_pull_requests_window(cx);
        self.sync_structure_window(cx);
        self.sync_todo_window(cx);
        self.sync_code_insight(cx);
        self.sync_agents_window(cx);
    }

    /// The view of tool window `id`, once it has been shown.
    pub fn tool_window_view(&self, id: &str) -> Option<AnyView> {
        match &self.content {
            Content::Project { panels, .. } => panels.get(id).map(|panel| panel.view.clone()),
            _ => None,
        }
    }

    /// Whether tool window `id` is showing.
    pub fn is_tool_window_visible(&self, id: &str) -> bool {
        matches!(&self.content, Content::Project { layout, .. } if layout.is_visible(id))
    }

    /// The run driver of the window's project, which starts and stops its processes.
    pub fn run_driver(&self) -> Option<&Entity<RunDriver>> {
        match &self.content {
            Content::Project { run, .. } => Some(run),
            _ => None,
        }
    }

    /// The run driver of a project just opened at `root`, listing its run configurations.
    fn new_run_driver(&mut self, root: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<RunDriver> {
        let store = cx.new(|_| RunStore::new());
        let weak = cx.weak_entity();
        let save_all: SaveAll = Rc::new(move |cx: &mut App| {
            use futures::FutureExt as _;
            match weak.upgrade() {
                Some(workbench) => workbench.read(cx).save_all(cx).boxed_local(),
                None => futures::future::ready(Ok(())).boxed_local(),
            }
        });
        let tool_windows = TOOL_WINDOWS
            .into_iter()
            .filter_map(tool_window_declaration)
            .map(|declared| (declared.id.clone(), declared.title.clone()))
            .collect();
        let (label, handle) = (self.label.clone(), window.window_handle());
        let driver = cx.new(|_| {
            RunDriver::new(&label, store).with_window(handle).with_save_all(save_all).with_tool_windows(tool_windows)
        });
        driver.update(cx, |driver, cx| driver.load(root, cx));
        self.mount_run_widget(&driver, window, cx);
        driver
    }

    /// The project goes: its processes stop and its sessions are forgotten.
    fn reset_runs(&mut self, cx: &mut Context<Self>) {
        self.drop_run_configs();
        if let Some(driver) = self.run_driver().cloned() {
            driver.update(cx, |driver, cx| driver.reset(cx));
        }
        self.reset_debugger(cx);
    }

    /// Tells the run driver whether the Run tool window shows, and the Run view what it reads from the
    /// settings and the registered actions.
    fn sync_run_window(&mut self, cx: &mut Context<Self>) {
        let Some(driver) = self.run_driver().cloned() else { return };
        let visible = self.is_tool_window_visible(RUN_TOOL_WINDOW);
        driver.update(cx, |driver, _| driver.run_window_visible = visible);
        let Some(view) = self.tool_window_view(RUN_TOOL_WINDOW).and_then(|view| view.downcast::<RunView>().ok()) else {
            return;
        };
        let console = crate::platform::settings::effective(&self.label, cx).console;
        let root = self.project_root(cx).map(|root| root.to_string_lossy().into_owned());
        let registered = |id: &str| cx.try_global::<ActionRegistry>().is_some_and(|registry| registry.contains(id));
        let (debug, edit) = (registered("run.debugConfig"), registered("run.editConfigs"));
        let chooser = registered("run.chooseConfig")
            .then(|| {
                let keymap = cx.try_global::<crate::platform::input::Keymap>()?;
                keymap.0.shortcut_label("run.chooseConfig", crate::platform::keymap::is_mac_host())
            })
            .flatten();
        view.update(cx, |view, cx| {
            view.set_console_settings(console, cx);
            view.set_project_root(root);
            view.set_capabilities(debug, edit, chooser, cx);
        });
    }

    /// Carries out what the run driver asks of the window.
    fn run_driver_event(
        &mut self,
        _: &Entity<RunDriver>,
        event: &RunDriverEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            RunDriverEvent::ShowRunWindow { focus } => {
                self.with_layout(window, cx, |layout| layout.show(RUN_TOOL_WINDOW, focus));
            }
            RunDriverEvent::Notify(request) => self.notify(request, cx),
            RunDriverEvent::ShowToolWindow(id) => self.with_layout(window, cx, |layout| layout.focus(&id)),
            RunDriverEvent::OpenFile { path, line, col } => self.open_project_file_at(path, line, col, window, cx),
            RunDriverEvent::RunAction { id, arg } => window.defer(cx, move |window, cx| {
                if !execute_action_from(ActionSource::Other, id, arg.as_ref(), window, cx) {
                    tracing::info!("{id} is not available in this window");
                }
            }),
        }
    }

    /// Carries out what the Run tool window asks for.
    fn run_view_event(
        &mut self,
        view: &Entity<RunView>,
        event: &RunViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(driver) = self.run_driver().cloned() else { return };
        match event.clone() {
            RunViewEvent::Rerun(session) => driver.update(cx, |driver, cx| {
                if let Some(rerun) = driver.rerun(&session, cx) {
                    rerun.detach();
                }
            }),
            RunViewEvent::Debug(config) => match serde_json::to_value(&config) {
                Ok(arg) => window.defer(cx, move |window, cx| {
                    execute_action_from(ActionSource::Toolbar, "run.debugConfig", Some(&arg), window, cx);
                }),
                Err(error) => tracing::error!("cannot pass the configuration to the debugger: {error}"),
            },
            RunViewEvent::Stop(session) => driver.update(cx, |driver, cx| driver.stop(&session, cx)),
            RunViewEvent::CloseTab(session) => driver.update(cx, |driver, cx| driver.close_tab(&session, cx)),
            RunViewEvent::EditConfigs(config) => window.defer(cx, move |window, cx| {
                let arg = config.map(serde_json::Value::from);
                execute_action_from(ActionSource::Toolbar, "run.editConfigs", arg.as_ref(), window, cx);
            }),
            RunViewEvent::SendInput { session, line } => {
                driver.update(cx, |driver, cx| driver.send_input(&session, &line, cx));
            }
            RunViewEvent::SendEof(session) => driver.update(cx, |driver, cx| driver.close_stdin(&session, cx)),
            RunViewEvent::Resize { session, cols, rows } => {
                driver.update(cx, |driver, cx| driver.resize_terminal(&session, cols, rows, cx));
            }
            RunViewEvent::OpenUrl(url) => driver.update(cx, |driver, cx| driver.open_url(&url, cx)),
            RunViewEvent::OpenLocation { target, cwd } => {
                driver.update(cx, |driver, cx| driver.open_location(target, cwd, cx));
            }
            RunViewEvent::ShowMenu(position) => {
                let items = view.read(cx).console_menu(cx);
                let view = view.clone();
                let run = move |_: &mut Self, command, window: &mut Window, cx: &mut Context<Self>| {
                    view.update(cx, |view, cx| view.run_console_command(command, window, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
        }
    }

    /// Stop: the target [`stop_target`] picks; several running processes are offered in a menu with
    /// Stop All.
    pub fn stop_runs(&mut self, focused: bool, from_widget: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(driver) = self.run_driver().cloned() else { return };
        let target = stop_target(driver.read(cx).store().read(cx), focused, from_widget);
        let sessions = match target {
            StopTarget::Nothing => return,
            StopTarget::Sessions(sessions) => sessions,
            StopTarget::Choose(sessions) => {
                let store = driver.read(cx).store().read(cx);
                let mut items: Vec<MenuItem<Vec<String>>> = sessions
                    .iter()
                    .filter_map(|id| store.session(id).map(|session| (id, session.config.name.clone())))
                    .map(|(id, name)| MenuItem::item(name, vec![id.clone()]).icon("run"))
                    .collect();
                items.push(MenuItem::Separator);
                items.push(MenuItem::item("Stop All", sessions).icon("stop").danger());
                let width = f32::from(window.viewport_size().width);
                let position = gpui_kit::point(gpui_kit::px((width / 2.0 - 120.0).max(0.0)), gpui_kit::px(44.0));
                let run = move |_: &mut Self, sessions: Vec<String>, _: &mut Window, cx: &mut Context<Self>| {
                    driver.update(cx, |driver, cx| sessions.iter().for_each(|id| driver.stop(id, cx)));
                };
                self.show_menu(&items, position, run, window, cx);
                return;
            }
        };
        driver.update(cx, |driver, cx| sessions.iter().for_each(|id| driver.stop(id, cx)));
    }

    /// The file tree, when a project is open.
    pub fn project_tree(&self) -> Option<&Entity<ProjectTree>> {
        match &self.content {
            Content::Project { tree, .. } => Some(tree),
            _ => None,
        }
    }

    /// Opens the folder at `root` as this window's project.
    /// Records that another editor became the active one: it is the most recently used tab, and
    /// its file the most recent of the project's recent files.
    fn note_active_tab(&mut self, cx: &mut Context<Self>) {
        let active = self
            .tabs()
            .and_then(|tabs| tabs.pane(&tabs.active_pane))
            .and_then(|pane| pane.active_tab())
            .map(|tab| (tab.input.key.clone(), tab.input.path.clone()));
        let key = active.as_ref().map(|(key, _)| key.clone());
        if key == self.active_key {
            return;
        }
        self.active_key = key;
        let Some((key, path)) = active else { return };
        let stamp = self.activations.values().max().copied().unwrap_or(0) + 1;
        self.activations.insert(key, stamp);
        if let (Some(path), Some(backend)) = (path, cx.try_global::<Backend>())
            && let Err(error) = ide_host::ops::nav::mark_recent(&backend.host, &self.label, PathBuf::from(path))
        {
            tracing::debug!("a file was not marked as recent: {}", error.message);
        }
    }

    /// Every open editor once, the most recently active first, as the Switcher lists them.
    fn open_editor_tabs(&self) -> Vec<OpenEditorTab> {
        let Some(tabs) = self.tabs() else { return Vec::new() };
        let mut seen = std::collections::HashSet::new();
        let mut open: Vec<OpenEditorTab> = tabs
            .panes
            .iter()
            .flat_map(|pane| &pane.tabs)
            .filter(|tab| seen.insert(tab.input.key.as_str()))
            .map(|tab| {
                let path = tab.input.path.as_deref().unwrap_or(&tab.input.key);
                let title = tab_views::title_of(&tab.input)
                    .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_owned());
                OpenEditorTab {
                    input: tab.input.clone(),
                    icon: tab_views::icon_of(&tab.input),
                    title,
                    last_active: self.activations.get(&tab.input.key).copied().unwrap_or(0),
                }
            })
            .collect();
        open.sort_by_key(|tab| std::cmp::Reverse(tab.last_active));
        open
    }

    /// Reads the bookmarks stored for the project at `root`.
    fn load_bookmarks(&mut self, root: &str, cx: &App) {
        let storage = cx.try_global::<UiStorage>();
        let stored = |key: String| storage.and_then(|storage| storage.get(&key));
        self.bookmarks.load(Some(root), stored(bookmarks_key(root)), stored(lists_key(root)));
    }

    pub fn bookmarks(&self) -> &BookmarkStore {
        &self.bookmarks
    }

    /// Saves what a change of the bookmarks says to save.
    fn save_bookmark_writes(writes: Vec<StorageWrite>, cx: &mut App) {
        if let Some(storage) = cx.try_global::<UiStorage>().is_some().then(|| cx.global_mut::<UiStorage>()) {
            for write in writes {
                storage.set(&write.key, write.value);
            }
        }
    }

    /// The file, caret line and that line's text of the active editor.
    fn caret_line(&self, cx: &App) -> Option<(String, u32, String)> {
        let editor = self.editor()?.read(cx);
        let line = editor.caret_position().0;
        Some((editor.path().to_string_lossy().into_owned(), line, editor.line_text(line)))
    }

    /// Adds a bookmark on `line` of `path`, or removes the one there. Without a place it is the
    /// caret's line of the active editor, described by a short selection.
    pub fn toggle_bookmark(&mut self, place: Option<(String, u32)>, cx: &mut Context<Self>) {
        let now = now_ms() as u64;
        let at = match place {
            Some((path, line)) => {
                let text = self
                    .editor()
                    .map(|editor| editor.read(cx))
                    .filter(|editor| editor.path().to_string_lossy() == path.as_str())
                    .map(|editor| editor.line_text(line))
                    .unwrap_or_default();
                Some((path, line, text, None))
            }
            None => self.caret_line(cx).map(|(path, line, text)| {
                let selected = self.editor().map(|editor| editor.read(cx).selected_text()).unwrap_or_default();
                (path, line, text, selection_description(&selected))
            }),
        };
        let Some((path, line, text, description)) = at else { return };
        let writes =
            self.bookmarks.update(|items| toggle_bookmark(items, &path, line, &text, now, description.as_deref()));
        Self::save_bookmark_writes(writes, cx);
        self.sync_bookmarks_window(cx);
        cx.notify();
    }

    /// Gives the caret's line the bookmark `digit`, or takes it away again.
    pub fn toggle_bookmark_digit(&mut self, digit: char, cx: &mut Context<Self>) {
        let Some((path, line, text)) = self.caret_line(cx) else { return };
        let now = now_ms() as u64;
        let writes = self.bookmarks.update(|items| toggle_mnemonic(items, &path, line, &text, digit, now));
        Self::save_bookmark_writes(writes, cx);
        self.sync_bookmarks_window(cx);
        cx.notify();
    }

    /// Bookmarks the given files and folders, or the active editor's file, or removes their bookmarks.
    pub fn toggle_file_bookmarks(&mut self, targets: Vec<(String, bool)>, cx: &mut Context<Self>) {
        let targets = if targets.is_empty() {
            self.editor()
                .map(|editor| (editor.read(cx).path().to_string_lossy().into_owned(), false))
                .into_iter()
                .collect()
        } else {
            targets
        };
        let now = now_ms() as u64;
        let borrowed: Vec<(&str, bool)> = targets.iter().map(|(path, dir)| (path.as_str(), *dir)).collect();
        let writes = self.bookmarks.update_data(|data| toggle_path_bookmarks(data, &borrowed, now));
        Self::save_bookmark_writes(writes, cx);
        self.sync_bookmarks_window(cx);
        cx.notify();
    }

    /// Goes to the bookmark after (or before) the caret, in file and line order, going round.
    pub fn go_to_neighbour_bookmark(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.caret_line(cx);
        let (path, line) = (at.as_ref().map(|at| at.0.as_str()), at.as_ref().map(|at| at.1));
        let Some(target) = neighbour_bookmark(&self.bookmarks.data().items, path, line, forward) else { return };
        let (path, line) = (PathBuf::from(&target.path), target.line);
        self.open_project_file_at(path, line, 0, window, cx);
    }

    /// Goes to the bookmark that has the mnemonic `digit`.
    pub fn go_to_bookmark(&mut self, digit: char, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = bookmark_by_mnemonic(&self.bookmarks.data().items, digit) else { return };
        let (path, line) = (PathBuf::from(&target.path), target.line);
        self.open_project_file_at(path, line, 0, window, cx);
    }

    /// Opens the Bookmarks popup; asked for again while it is open, it stays as it is.
    pub fn show_bookmarks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.popup.as_ref().is_some_and(|popup| popup.view.clone().downcast::<BookmarksPopup>().is_ok()) {
            return;
        }
        let (label, store) = (self.label.clone(), self.bookmarks.clone());
        let popup = cx.new(|cx| BookmarksPopup::new(&label, store, cx));
        let events = cx.subscribe_in(&popup, window, |workbench, popup, event: &BookmarksPopupEvent, window, cx| {
            match event.clone() {
                BookmarksPopupEvent::Dismissed => workbench.close_popup(window, cx),
                BookmarksPopupEvent::Open { path, line } => {
                    workbench.close_popup(window, cx);
                    match line {
                        Some(line) => workbench.open_project_file_at(path, line, 0, window, cx),
                        None => workbench.open_project_file(path, window, cx),
                    }
                }
                BookmarksPopupEvent::SelectInProjectView(path) => {
                    workbench.close_popup(window, cx);
                    let arg = serde_json::json!({ "path": path.to_string_lossy() });
                    window.defer(cx, move |window, cx| {
                        execute_action_from(
                            ActionSource::Other,
                            "navigate.selectInProjectView",
                            Some(&arg),
                            window,
                            cx,
                        );
                    });
                }
                // The popup changed its own copy: the window takes it over and saves it.
                BookmarksPopupEvent::Changed(writes) => {
                    workbench.bookmarks = popup.read(cx).store().clone();
                    Self::save_bookmark_writes(writes, cx);
                    workbench.sync_bookmarks_window(cx);
                }
            }
        });
        self.open_popup(&popup, events, window, cx);
    }

    /// Opens the chooser that gives the caret's line a bookmark with a mnemonic.
    pub fn show_mnemonic_chooser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, line, text)) = self.caret_line(cx) else { return };
        let target = MnemonicTarget { path, line, text, at: None };
        let store = self.bookmarks.clone();
        let chooser = cx.new(|cx| MnemonicChooser::new(store, target, cx));
        let events =
            cx.subscribe_in(&chooser, window, |workbench, chooser, event: &MnemonicChooserEvent, window, cx| {
                if let MnemonicChooserEvent::Done(writes) = event.clone() {
                    workbench.bookmarks = chooser.read(cx).store().clone();
                    Self::save_bookmark_writes(writes, cx);
                    workbench.sync_bookmarks_window(cx);
                }
                workbench.close_popup(window, cx);
            });
        self.open_popup(&chooser, events, window, cx);
    }

    /// Opens Go to Line:Column for the active editor; the chosen place gets the caret.
    pub fn show_goto_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let position = self.editor().map(|editor| editor.read(cx).caret_position());
        let popup = cx.new(|cx| GotoLine::new(position, cx));
        let events = cx.subscribe_in(&popup, window, |workbench, _, event: &GotoLineEvent, window, cx| {
            workbench.close_popup(window, cx);
            if let GotoLineEvent::Go { line, col } = *event
                && let Some(editor) = workbench.editor().cloned()
            {
                editor.update(cx, |editor, cx| {
                    // The line and the column are held inside the document.
                    let at = editor.offset_of(line, col);
                    editor.select(at, at, cx);
                    editor.scroll_to_line(line.saturating_sub(5), cx);
                });
            }
        });
        self.open_popup(&popup, events, window, cx);
    }

    pub fn find_popup(&self) -> Option<Entity<FindPopup>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    /// Opens Find in Files (or Replace in Files), starting with the editor's selection; asked
    /// for again while it is open, the open popup takes the new mode and argument.
    pub fn show_find_popup(&mut self, replace: bool, arg: FindArg, window: &mut Window, cx: &mut Context<Self>) {
        let selection = self.editor().map(|editor| editor.read(cx).selected_text()).filter(|text| !text.is_empty());
        if let Some(open) = self.find_popup() {
            open.update(cx, |popup, cx| popup.show(replace, &arg, selection.as_deref(), window, cx));
            return;
        }
        let input = FindPopupInput {
            window_label: self.label.clone(),
            root: self.project_root(cx).map(|root| root.to_string_lossy().into_owned()),
            workspace_folders: Vec::new(),
            replace,
            arg,
            selection,
            active_path: self.editor().map(|editor| editor.read(cx).path().to_string_lossy().into_owned()),
            open_paths: self.open_editor_tabs().into_iter().filter_map(|tab| tab.input.path).collect(),
            modules: Vec::new(),
        };
        let popup = cx.new(|cx| FindPopup::new(input, cx));
        let events = cx.subscribe_in(&popup, window, |workbench, popup, event: &FindPopupEvent, window, cx| {
            match event.clone() {
                FindPopupEvent::Dismissed => workbench.close_popup(window, cx),
                FindPopupEvent::Open { path, line, col, keep_open, .. } => {
                    if keep_open {
                        // A pinned popup stays, with the keyboard.
                        workbench.open_project_file_at(path, line, col, window, cx);
                        window.focus(&popup.focus_handle(cx), cx);
                    } else {
                        workbench.close_popup(window, cx);
                        workbench.open_project_file_at(path, line, col, window, cx);
                    }
                }
                FindPopupEvent::OpenInFindWindow { query, .. } => {
                    workbench.close_popup(window, cx);
                    workbench.with_layout(window, cx, |layout| layout.show(FIND_TOOL_WINDOW, true));
                    let panel = workbench
                        .tool_window_view(FIND_TOOL_WINDOW)
                        .and_then(|view| view.downcast::<crate::find::view::FindPanel>().ok());
                    if let Some(panel) = panel {
                        panel.update(cx, |panel, cx| panel.run_query(query, cx));
                    }
                }
                // No scope editor exists yet for the popup to send the user to.
                FindPopupEvent::EditScopes { .. } => {}
                FindPopupEvent::Notify(request) => workbench.notify(request, cx),
            }
        });
        self.open_popup(&popup, events, window, cx);
    }

    /// Opens Recent Files or the Switcher. Recent Files asked for again shows only the files
    /// edited in this session, and back; the Switcher asked for again moves its selection.
    pub fn show_recent_files(&mut self, kind: RecentFilesKind, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.recent_files() {
            let switcher = open.read(cx).is_switcher();
            match kind {
                RecentFilesKind::Switcher { backward, .. } if switcher => {
                    open.update(cx, |popup, cx| popup.cycle(if backward { -1 } else { 1 }, cx));
                    return;
                }
                RecentFilesKind::Recent if !switcher => {
                    open.update(cx, RecentFiles::toggle_edited_only);
                    return;
                }
                _ => {}
            }
        }
        let input = RecentFilesInput {
            window_label: self.label.clone(),
            root: self.project_root(cx).map(|root| root.to_string_lossy().into_owned()),
            active_path: self.editor().map(|editor| editor.read(cx).path().to_string_lossy().into_owned()),
            tabs: self.open_editor_tabs(),
            tool_windows: self.tool_windows().map(|layout| layout.windows().cloned().collect()).unwrap_or_default(),
            ..RecentFilesInput::default()
        };
        let popup = cx.new(|cx| RecentFiles::new(kind, input, cx));
        let events = cx.subscribe_in(&popup, window, |workbench, popup, event: &RecentFilesEvent, window, cx| {
            let popup = popup.clone();
            match event.clone() {
                RecentFilesEvent::Dismissed | RecentFilesEvent::ShowRecentLocations => {
                    workbench.close_popup(window, cx)
                }
                RecentFilesEvent::OpenFile { path, .. } => {
                    workbench.close_popup(window, cx);
                    workbench.open_project_file(path, window, cx);
                }
                RecentFilesEvent::OpenFiles { paths, lead, .. } => {
                    workbench.close_popup(window, cx);
                    for path in paths {
                        workbench.open_project_file(path, window, cx);
                    }
                    // The one the selection was on is the one that shows.
                    workbench.open_project_file(lead, window, cx);
                }
                RecentFilesEvent::OpenEditor { input, .. } => {
                    workbench.close_popup(window, cx);
                    workbench.with_tabs(window, cx, |tabs, _| {
                        let found =
                            tabs.find_tab(&input.key, None).map(|(pane, tab)| (pane.to_owned(), tab.id.clone()));
                        found.is_some_and(|(pane, tab)| tabs.set_active_pane(&pane) | tabs.activate_tab(&pane, &tab))
                    });
                }
                RecentFilesEvent::ActivateToolWindow(id) => {
                    workbench.close_popup(window, cx);
                    workbench.with_layout(window, cx, |layout| layout.show(&id, true));
                }
                RecentFilesEvent::HideToolWindow(id) => {
                    workbench.with_layout(window, cx, |layout| layout.hide(&id));
                    // Hiding the window that had the keyboard hands it to the editor: the popup is
                    // still open and keeps it.
                    window.focus(&popup.focus_handle(cx), cx);
                }
                RecentFilesEvent::ClosePath(key) | RecentFilesEvent::CloseInput(key) => {
                    if let Content::Project { tabs, .. } = &mut workbench.content {
                        tabs.close_key(&key);
                    }
                    // Without taking the keyboard: the popup stays open.
                    workbench.tabs_changed(false, window, cx);
                    popup.update(cx, |popup, cx| popup.removed(&key, cx));
                }
                RecentFilesEvent::GoToFile(query) => {
                    workbench.close_popup(window, cx);
                    workbench.open_search_everywhere(SeTab::Files, Some(query), window, cx);
                }
                RecentFilesEvent::ShowSwitcher { backward } => {
                    let control_held = window.modifiers().control;
                    workbench.show_recent_files(RecentFilesKind::Switcher { backward, control_held }, window, cx);
                }
            }
        });
        self.open_popup(&popup, events, window, cx);
    }

    pub fn recent_files(&self) -> Option<Entity<RecentFiles>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    /// Whether the status bar is drawn: there is something for it to say and View › Appearance ›
    /// Status Bar is on.
    pub fn shows_status_bar(&self, cx: &App) -> bool {
        (self.editor().is_some() || self.task_status().is_some())
            && crate::platform::settings::global(cx).appearance.show_status_bar
            && !self.distraction_free(cx)
    }

    /// Whether the window shows nothing: no project, no file.
    pub fn is_empty(&self) -> bool {
        matches!(self.content, Content::Empty | Content::Failed(_))
    }

    /// What is unsaved is saved and the tabs are remembered, before the window shows something else.
    fn leave(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.leave_decided) {
            self.autosave(None, cx);
        }
        self.save_session(cx);
        self.popup = None;
        self.drop_settings();
    }

    /// Closes the window's project; the window stays, showing the Welcome screen.
    pub fn close_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.project_tree().is_none() {
            return;
        }
        self.leave(cx);
        self.reset_runs(cx);
        self.reset_build(window, cx);
        self.reset_database(window, cx);
        self.drop_vcs();
        self.drop_commit();
        self.drop_git_log();
        self.reset_code_insight(cx);
        self.drop_agents(cx);
        self.drop_plugin_tool_windows();
        // The project's servers and runs stop in the background: this is the UI thread.
        self.drop_github();
        let root = cx.global::<Backend>().host.project(&self.label).ok().map(|project| project.root.clone());
        cx.global::<Backend>().host.close_project_detached(&self.label);
        // The project's own plugins go with it.
        if let Some(root) = root.filter(|_| cx.has_global::<crate::plugins::Plugins>()) {
            crate::plugins::Plugins::release_project(root, cx);
        }
        self.content = Content::Empty;
        self.light_find_bar = None;
        self.sync_welcome(window, cx);
        cx.notify();
    }

    /// The Welcome screen, while the window shows it.
    pub fn welcome(&self) -> Option<&Entity<Welcome>> {
        match self.content {
            Content::Empty => self.welcome.as_ref().map(|(welcome, _)| welcome),
            _ => None,
        }
    }

    /// Keeps a Welcome screen while the window shows nothing, listing the recent projects as they
    /// are stored now, and drops it once a project or a file opens. Without a backend there are no
    /// recent projects to show.
    fn sync_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_empty() {
            self.welcome = None;
            self.drop_welcome_plugins();
            return;
        }
        let Some(backend) = cx.try_global::<Backend>() else { return };
        let recent = ide_workspace::RecentProjects::load(&backend.dirs.config);
        let (list, groups) = (recent.list(), recent.groups());
        if let Some((welcome, _)) = &self.welcome {
            welcome.update(cx, |view, cx| view.set_recent(&list, &groups, cx));
            // A plugin host that started after the window, or a Welcome screen first made behind a
            // failed open and shown now, gets its listener, its Plugins page and its settings here.
            self.listen_to_plugins(window, cx);
            self.sync_customize(cx);
            self.sync_welcome_plugins(window, cx);
            return;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let welcome =
            cx.new(|cx| Welcome::new(&list, &groups, home.as_deref(), cx).with_version(env!("CARGO_PKG_VERSION")));
        let events = cx.subscribe_in(&welcome, window, Self::welcome_event);
        // When it is what the window shows, it takes the keyboard: typing searches the recent
        // projects. Behind a failure's message it is not drawn and leaves the keys alone.
        if matches!(self.content, Content::Empty) {
            welcome.update(cx, |view, cx| view.focus_search(window, cx));
        }
        self.welcome = Some((welcome, events));
        self.sync_customize(cx);
        self.sync_welcome_plugins(window, cx);
    }

    /// Carries out what the Welcome screen asks for.
    fn welcome_event(
        &mut self,
        welcome: &Entity<Welcome>,
        event: &WelcomeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = cx.global::<Backend>().dirs.config.clone();
        match event.clone() {
            // Once this update is over: opening replaces what the window shows.
            WelcomeEvent::Open(path) => window.defer(cx, move |window, cx| {
                if let Err(error) = windows::open_project_with_policy(&path, window, cx) {
                    tracing::error!("cannot open {}: {error:#}", path.display());
                }
            }),
            WelcomeEvent::RunAction(id) => window.defer(cx, move |window, cx| {
                execute_action_from(ActionSource::Other, id, None, window, cx);
            }),
            WelcomeEvent::Remove(path) => {
                match ide_workspace::RecentProjects::load(&config).remove(&path) {
                    Ok(()) => menu::refresh_recent(cx),
                    Err(error) => {
                        let mut request =
                            NotificationRequest::new(NotificationLevel::Error, "Cannot remove recent project");
                        request.body = Some(error.message);
                        self.notify(request, cx);
                    }
                }
                // The list shows what is stored, also when forgetting failed.
                self.sync_welcome(window, cx);
            }
            WelcomeEvent::SetGroups(groups) => {
                let recent = ide_workspace::RecentProjects::load(&config);
                match recent.set_groups(groups) {
                    Ok(groups) => {
                        let list = recent.list();
                        welcome.update(cx, |view, cx| view.set_recent(&list, &groups, cx));
                    }
                    Err(error) => {
                        let mut request =
                            NotificationRequest::new(NotificationLevel::Error, "Cannot save project groups");
                        request.body = Some(error.message);
                        self.notify(request, cx);
                        self.sync_welcome(window, cx);
                    }
                }
            }
            WelcomeEvent::Reveal(path) => cx.reveal_path(&path),
            WelcomeEvent::Customize(event) => self.customize_event(event, window, cx),
            WelcomeEvent::ShowMenu { target, position } => {
                let items = welcome.update(cx, |view, cx| view.menu(&target, cx));
                let welcome = welcome.downgrade();
                let run = move |_: &mut Self, command, window: &mut Window, cx: &mut Context<Self>| {
                    // The view is gone when the window shows a project by now.
                    let _ = welcome.update(cx, |view, cx| view.run(command, window, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
        }
    }

    /// Carries out what the Welcome screen's Customize page asks: the global settings writes (a
    /// failure is reported; either way the page then shows the settings in effect), the reopen
    /// preference kept on this machine, and its links' actions.
    fn customize_event(&mut self, event: welcome_pages::CustomizeEvent, window: &mut Window, cx: &mut Context<Self>) {
        use welcome_pages::CustomizeEvent;
        match event {
            CustomizeEvent::Write(writes) => {
                if let Err(error) = crate::platform::settings::set_global(&self.label, &writes, cx) {
                    let key = writes.first().map_or("", |write| write.key);
                    let request = NotificationRequest {
                        body: Some(error.message),
                        key: Some(format!("welcome.setting:{key}")),
                        ..NotificationRequest::new(NotificationLevel::Error, "Cannot save setting")
                    };
                    self.notify(request, cx);
                }
                self.sync_customize(cx);
            }
            CustomizeEvent::ReopenPreference(on) => {
                cx.default_global::<UiStorage>().set(REOPEN_PREFERENCE, on.to_string());
            }
            CustomizeEvent::RunAction(id) => window.defer(cx, move |window, cx| {
                execute_action_from(ActionSource::Other, id, None, window, cx);
            }),
        }
    }

    /// Gives the Welcome screen's Customize page the settings in effect for the window and the
    /// reopen preference of this machine (on unless it was turned off).
    pub(super) fn sync_customize(&self, cx: &mut Context<Self>) {
        use crate::settings_model::schema::settings_value;
        let Some(welcome) = self.welcome().cloned() else { return };
        let settings = settings_value(&crate::platform::settings::effective(&self.label, cx));
        let preference =
            cx.try_global::<UiStorage>().and_then(|storage| storage.get(REOPEN_PREFERENCE)) != Some("false");
        let customize = welcome.read(cx).customize().clone();
        customize.update(cx, |page, cx| page.set_settings(settings, preference, cx));
    }

    pub fn open_project(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.leave(cx);
        self.reset_runs(cx);
        self.reset_build(window, cx);
        self.reset_database(window, cx);
        self.drop_vcs();
        self.drop_commit();
        self.drop_git_log();
        self.reset_code_insight(cx);
        self.drop_agents(cx);
        self.drop_plugin_tool_windows();
        self.drop_github();
        self.files = Default::default();
        self.content = Content::Opening(root.clone());
        self.light_find_bar = None;
        self.sync_welcome(window, cx);
        cx.notify();
        let label = self.label.clone();
        let config = cx.global::<Backend>().dirs.config.clone();
        let opened = cx.global::<Backend>().with_host(move |host| async move {
            let project = ide_host::ops::project::open(&host, &label, &root).await?;
            let info = ide_host::ops::project::info(&project);
            // An opened project is a recent one, under the name it shows.
            let attached = info.workspace.as_ref().is_some_and(|workspace| !workspace.is_container(&project.root));
            let branch = (!attached)
                .then(|| project.repos.read().primary().and_then(|repo| repo.branch_info().ok()).and_then(|b| b.head))
                .flatten();
            let recent = ide_workspace::RecentProjects::load(&config);
            recent.touch(&project.root, branch)?;
            recent.rename(&project.root, Some(&info.display_name))?;
            let name = if info.display_name.is_empty() { info.name } else { info.display_name };
            ide_core::IdeResult::Ok((project.root.clone(), name))
        });
        cx.spawn_in(window, async move |workbench, cx| {
            let result = opened.await;
            let _ = workbench.update_in(cx, |workbench, window, cx| {
                let opened =
                    result.map_err(|error| error.to_string()).and_then(|opened| opened.map_err(|error| error.message));
                workbench.content = match opened {
                    Ok((root, name)) => {
                        // It is the most recent project now: Open Recent lists it first.
                        menu::refresh_recent(cx);
                        workbench.load_bookmarks(&root.to_string_lossy(), cx);
                        let label = workbench.label.clone();
                        // The layout is stored under the project folder's path as its key.
                        let scope = root.to_string_lossy().into_owned();
                        let stored =
                            cx.try_global::<UiStorage>().and_then(|s| s.get(&storage_key(&scope)).map(str::to_owned));
                        let mut layout = Box::new(ToolWindowLayout::new());
                        layout.load(&scope, stored.as_deref());
                        // In Distraction Free Mode the project's tool windows are hidden like the others'.
                        if app_actions::zen_layout_loaded(&mut layout, cx) {
                            workbench.show_distraction_free_hint(cx);
                        }
                        let run = workbench.new_run_driver(&root.to_string_lossy(), window, cx);
                        let run_events = cx.subscribe_in(&run, window, Self::run_driver_event);
                        workbench.mount_debugger(&root.to_string_lossy(), window, cx);
                        let tree = cx.new(|cx| ProjectTree::new(&label, root, cx));
                        let opens = cx.subscribe_in(&tree, window, |workbench, _, event: &OpenFile, window, cx| {
                            workbench.open_project_file(event.0.clone(), window, cx);
                        });
                        let menus = cx.subscribe_in(&tree, window, Self::tree_context_menu);
                        let project_panel = Panel::new(tree.clone(), Some(menus));
                        // The tabs, splits and carets the project was left with.
                        let saved = cx.try_global::<UiStorage>().and_then(|s| session::load_session(s, &scope));
                        let (tabs, restored_views) = match saved {
                            Some(saved) => (session::restore_tabs_state(&saved, |path| path.to_owned()), saved.files),
                            None => (TabsState::new(), Default::default()),
                        };
                        Content::Project {
                            name,
                            layout,
                            focus_served: 0,
                            tree,
                            panels: HashMap::from([(PROJECT_TOOL_WINDOW.to_owned(), project_panel)]),
                            tabs: Box::new(tabs),
                            editors: HashMap::new(),
                            loading: Default::default(),
                            restored_views,
                            reveals: HashMap::new(),
                            editor_focus: HashMap::new(),
                            find_bars: HashMap::new(),
                            view_tabs: Default::default(),
                            _opens: opens,
                            run,
                            _run_events: run_events,
                        }
                    }
                    Err(message) => Content::Failed(message.into()),
                };
                workbench.mount_vcs(window, cx);
                workbench.mount_agents(window, cx);
                if workbench.project_root(cx).is_some() {
                    workbench.code_insight_project_opened(cx);
                }
                workbench.sync_welcome(window, cx);
                // Registering a window reconciles the stored layout with what exists; a project
                // opens with the focus in its Project view when that is showing.
                workbench.with_layout(window, cx, |layout| {
                    let mut effects = Effects::default();
                    let mut merge = |more: Effects| {
                        effects.persist |= more.persist;
                        effects.focus_editor |= more.focus_editor;
                    };
                    for declared in TOOL_WINDOWS.into_iter().filter_map(tool_window_declaration) {
                        merge(layout.register(declared).1);
                    }
                    if layout.is_visible(PROJECT_TOOL_WINDOW) {
                        merge(layout.focus(PROJECT_TOOL_WINDOW));
                    }
                    effects
                });
                // The views plugins declare register after the IDE's own windows, reconciled the same way.
                workbench.mount_plugin_tool_windows(window, cx);
                // A restored tab gets its editor when it is the one showing.
                workbench.tabs_changed(false, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens a file of the project with the caret at the zero-based `line` and UTF-16 column
    /// `col`: a search match, a commit's file, an error's location.
    pub fn open_project_file_at(
        &mut self,
        path: PathBuf,
        line: u32,
        col: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = path.to_string_lossy().into_owned();
        if let Content::Project { reveals, .. } = &mut self.content {
            reveals.insert(key, (line, col));
        }
        self.open_project_file(path, window, cx);
        self.serve_reveal(cx);
    }

    /// Shows `view` over the window and gives it the keyboard. One popup is open at a time.
    fn open_popup<V: Focusable + Render>(
        &mut self,
        view: &Entity<V>,
        events: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_popup(window, cx);
        let restore = window.focused(cx);
        window.focus(&view.focus_handle(cx), cx);
        self.popup = Some(Popup { view: view.clone().into(), restore, _events: events });
        cx.notify();
    }

    /// Closes the popup. The keyboard goes back to where it was when the popup opened, or to the
    /// editor, and the context keys say so at once: what the popup chose to do next is judged
    /// against the window as it is without it.
    pub fn close_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popup) = self.popup.take() else { return };
        self.run_popup_closed(&popup.view, cx);
        self.vcs_popup_closed(&popup.view, cx);
        self.structure_popup_closed(&popup.view, cx);
        self.files_popup_closed(&popup.view);
        self.usages_popup_closed(&popup.view, cx);
        if let Some(target) = popup.restore.or_else(|| self.editor().map(|editor| editor.focus_handle(cx))) {
            window.focus(&target, cx);
        }
        self.publish_context(window, cx);
        cx.notify();
    }

    /// Opens a menu of `items` with its corner at `position`. A chosen action runs in the window;
    /// a chosen row of the menu's own goes to `on_command`. Either way the menu closes first, so
    /// what was chosen finds the window as it was.
    pub fn show_menu<C: Clone + 'static>(
        &mut self,
        items: &[MenuItem<C>],
        position: gpui_kit::Point<gpui_kit::Pixels>,
        on_command: impl Fn(&mut Self, C, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = cx.new(|cx| PopupMenu::new(items, position, window, cx));
        self.show_popup_menu(menu, on_command, window, cx);
    }

    /// Shows `menu`, built by the caller, as [`Workbench::show_menu`] shows the menu it builds.
    pub(super) fn show_popup_menu<C: Clone + 'static>(
        &mut self,
        menu: Entity<PopupMenu<C>>,
        on_command: impl Fn(&mut Self, C, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let events = cx.subscribe_in(&menu, window, move |workbench, _, event: &PopupMenuEvent<C>, window, cx| {
            workbench.close_popup(window, cx);
            match event.clone() {
                PopupMenuEvent::Dismissed => {}
                PopupMenuEvent::Chosen(Choice::Command(command)) => on_command(workbench, command, window, cx),
                PopupMenuEvent::Chosen(Choice::Action { id, arg }) => {
                    // Once this update is over: the action may reach for the workbench.
                    window.defer(cx, move |window, cx| {
                        execute_action_from(ActionSource::ContextMenu, &id, arg.as_ref(), window, cx);
                    });
                }
            }
        });
        self.open_popup(&menu, events, window, cx);
    }

    /// The menu that is open, when it is one of commands `C`.
    pub fn open_menu<C: 'static>(&self) -> Option<Entity<PopupMenu<C>>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    /// The menu of the tab `tab` of `pane`: closing it and its neighbours, pinning, splitting,
    /// and what can be done with its file.
    pub fn tab_menu(&self, pane: &str, tab: &str, window: &Window, cx: &App) -> Vec<MenuItem<TabCommand>> {
        let Some(tabs) = self.tabs() else { return Vec::new() };
        let Some(open) = tabs.pane(pane) else { return Vec::new() };
        let Some(index) = open.tabs.iter().position(|other| other.id == tab) else { return Vec::new() };
        let (this, count) = (&open.tabs[index], open.tabs.len());
        let shortcut = |action: &str| {
            cx.try_global::<input::Keymap>()
                .and_then(|keymap| keymap.0.shortcut_label(action, crate::platform::keymap::is_mac_host()))
        };
        let row = |label: &str, command: TabCommand, enabled: bool| MenuItem::entry(label, enabled.then_some(command));
        let with_shortcut = |item: MenuItem<TabCommand>, action: &str| match (item, shortcut(action)) {
            (MenuItem::Item(mut entry), Some(shortcut)) => {
                entry.shortcut = Some(shortcut);
                MenuItem::Item(entry)
            }
            (item, _) => item,
        };
        let mut items = vec![
            with_shortcut(row("Close", TabCommand::Close, true), "editor.closeTab"),
            row("Close Other Tabs", TabCommand::CloseOthers, count >= 2),
            row("Close Tabs to the Left", TabCommand::CloseLeft, index > 0),
            row("Close Tabs to the Right", TabCommand::CloseRight, index + 1 < count),
            row("Close All Tabs", TabCommand::CloseAll, true),
            row("Close Unmodified Tabs", TabCommand::CloseUnmodified, true),
        ];
        if open.tabs.iter().any(|other| other.pinned) {
            items.push(row("Close All but Pinned", TabCommand::CloseAllButPinned, true));
        }
        let read_only = self.read_only_tabs(cx);
        if open.tabs.iter().any(|other| !other.pinned && other.input.path.is_some() && read_only.contains(&other.id)) {
            items.push(row("Close All Read-Only Tabs", TabCommand::CloseReadOnly, true));
        }
        items.push(MenuItem::Separator);
        items.push(row(if this.pinned { "Unpin Tab" } else { "Pin Tab" }, TabCommand::TogglePin, true).icon("pin"));
        if this.preview {
            items.push(row("Keep Tab Open", TabCommand::Keep, true));
        }
        items.extend([
            MenuItem::Separator,
            row("Split and Move Right", TabCommand::SplitAndMoveRight, true),
            row("Split and Move Down", TabCommand::SplitAndMoveDown, true),
            with_shortcut(row("Split Right", TabCommand::SplitRight, true).icon("split"), "editor.splitRight"),
            with_shortcut(row("Split Down", TabCommand::SplitDown, true).icon("split-horizontal"), "editor.splitDown"),
        ]);
        if tabs.panes.len() > 1 {
            items.extend([
                row("Move to Opposite Group", TabCommand::MoveToOpposite, true),
                row("Open in Opposite Group", TabCommand::OpenInOpposite, true),
                row("Change Splitter Orientation", TabCommand::ChangeOrientation, true),
                row("Unsplit", TabCommand::Unsplit, true),
                row("Unsplit All", TabCommand::UnsplitAll, true),
            ]);
        }
        items.push(MenuItem::Separator);
        if let Some(path) = this.input.path.clone().filter(|_| this.input.kind == "file") {
            let in_project = self.project_root(cx).is_some_and(|root| std::path::Path::new(&path).starts_with(root));
            let mut copy = vec![
                MenuItem::action_with("project.copyAbsolutePath", path.clone().into()).labelled("Absolute Path"),
                MenuItem::action_with("project.copyFileName", path.clone().into()).labelled("File Name"),
            ];
            if in_project {
                copy.push(
                    MenuItem::action_with("project.copyRelativePath", path.clone().into())
                        .labelled("Path From Content Root"),
                );
            }
            let text = match &self.content {
                Content::Project { editors, .. } => editors.get(tab).map(|editor| editor.read(cx).text()),
                _ => None,
            };
            items.extend(self.tab_file_rows(&path, in_project, text.as_deref(), window, cx));
            items.extend([
                MenuItem::action_with("diff.compareWith", path.clone().into()).labelled("Compare With…"),
                MenuItem::action_with("diff.compareWithClipboard", serde_json::json!({ "path": path }))
                    .labelled("Compare with Clipboard"),
                MenuItem::Separator,
                MenuItem::submenu("Copy Path/Reference…", copy),
                MenuItem::action_with("project.rename", path.into()).labelled("Rename File…"),
                MenuItem::Separator,
            ]);
        }
        items.push(
            MenuItem::action_with("file.settings", "editor.general.editorTabs".into())
                .labelled("Configure Editor Tabs…"),
        );
        items
    }

    /// The ids of the tabs whose editor shows a read-only document.
    fn read_only_tabs(&self, cx: &App) -> std::collections::HashSet<String> {
        match &self.content {
            Content::Project { editors, .. } => {
                editors.iter().filter(|(_, editor)| editor.read(cx).is_read_only()).map(|(id, _)| id.clone()).collect()
            }
            _ => Default::default(),
        }
    }

    /// Carries out a row of the menu of the tab `tab` of `pane`.
    pub fn run_tab_command(
        &mut self,
        pane: &str,
        tab: &str,
        command: TabCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (pane, tab) = (pane.to_owned(), tab.to_owned());
        // The splits copy or move the tab the menu was opened on: it is the active one first.
        let activate = |workbench: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            workbench.with_tabs(window, cx, |tabs, _| tabs.set_active_pane(&pane) | tabs.activate_tab(&pane, &tab));
        };
        match command {
            TabCommand::SplitRight | TabCommand::SplitDown => {
                activate(self, window, cx);
                let dir = if command == TabCommand::SplitRight { SplitDir::Row } else { SplitDir::Column };
                return self.split_editor(dir, window, cx);
            }
            TabCommand::MoveToOpposite => {
                activate(self, window, cx);
                return self.move_tab_to_opposite_group(window, cx);
            }
            TabCommand::OpenInOpposite => {
                activate(self, window, cx);
                return self.open_in_opposite_group(window, cx);
            }
            _ => {}
        }
        let dirty: std::collections::HashSet<String> = match &self.content {
            Content::Project { editors, view_tabs, .. } => editors
                .iter()
                .filter(|(_, editor)| editor.read(cx).is_dirty())
                .map(|(id, _)| id.clone())
                .chain(view_tabs.dirty_tabs(cx))
                .collect(),
            _ => Default::default(),
        };
        let read_only = self.read_only_tabs(cx);
        self.with_tabs(window, cx, |tabs, _| match command {
            TabCommand::Close => tabs.close_tab(&pane, &tab),
            TabCommand::CloseOthers => tabs.close_others(&pane, &tab),
            TabCommand::CloseLeft => tabs.close_to_left(&pane, &tab),
            TabCommand::CloseRight => tabs.close_to_right(&pane, &tab),
            TabCommand::CloseAll => tabs.close_all(&pane, false),
            TabCommand::CloseAllButPinned => tabs.close_all(&pane, true),
            TabCommand::CloseReadOnly => tabs.close_unmodified(&pane, |tab| !read_only.contains(&tab.id)),
            TabCommand::CloseUnmodified => tabs.close_unmodified(&pane, |tab| dirty.contains(&tab.id)),
            TabCommand::TogglePin => tabs.toggle_pin(&pane, &tab),
            TabCommand::Keep => tabs.promote_tab(&pane, &tab),
            TabCommand::SplitAndMoveRight => tabs.move_tab_to_new_split(&pane, &tab, &pane, SplitEdge::Right).is_some(),
            TabCommand::SplitAndMoveDown => tabs.move_tab_to_new_split(&pane, &tab, &pane, SplitEdge::Bottom).is_some(),
            TabCommand::ChangeOrientation => tabs.change_split_orientation(&pane),
            TabCommand::Unsplit => tabs.remove_pane(&pane),
            TabCommand::UnsplitAll => tabs.unsplit_all(&pane),
            TabCommand::SplitRight
            | TabCommand::SplitDown
            | TabCommand::MoveToOpposite
            | TabCommand::OpenInOpposite => false,
        });
    }

    /// Opens Search Everywhere on `tab`, or moves the open one to it. A short selection on one
    /// line of the editor is what it starts searching for.
    pub fn show_search_everywhere(&mut self, tab: SeTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.search_everywhere() {
            open.update(cx, |popup, cx| popup.select_tab(tab, cx));
            return;
        }
        let selected = self.editor().map(|editor| editor.read(cx).selected_text());
        let query = selected.filter(|text| {
            !text.is_empty() && text.encode_utf16().count() <= SELECTION_QUERY_LIMIT && !text.contains('\n')
        });
        self.open_search_everywhere(tab, query, window, cx);
    }

    /// Opens Search Everywhere on `tab`, searching for `query`.
    fn open_search_everywhere(
        &mut self,
        tab: SeTab,
        query: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = self.project_root(cx).map(|root| root.to_string_lossy().into_owned());
        let context = crate::platform::context::window_context(window, cx);
        let label = self.label.clone();
        let popup = cx.new(|cx| SearchEverywhere::new(&label, tab, query.as_deref(), root, context, cx));
        let events = cx.subscribe_in(&popup, window, |workbench, _, event: &SearchEverywhereEvent, window, cx| {
            match event {
                SearchEverywhereEvent::Dismissed => workbench.close_popup(window, cx),
                SearchEverywhereEvent::OpenFile { path, position } => {
                    workbench.close_popup(window, cx);
                    match position {
                        Some((line, col)) => workbench.open_project_file_at(path.clone(), *line, *col, window, cx),
                        None => workbench.open_project_file(path.clone(), window, cx),
                    }
                }
                SearchEverywhereEvent::RunAction(id) => {
                    let (id, workbench) = (id.clone(), cx.weak_entity());
                    // Once this update is over: the action, and whether it shows a state, may
                    // reach for the workbench.
                    window.defer(cx, move |window, cx| {
                        let toggle = cx
                            .global::<ActionRegistry>()
                            .get(&id)
                            .is_some_and(|action| action.is_toggled(window, cx).is_some());
                        // An action that shows a state stays in the list, where its new state shows.
                        if !toggle {
                            let _ = workbench.update(cx, |workbench, cx| workbench.close_popup(window, cx));
                        }
                        execute_action_from(ActionSource::Search, &id, None, window, cx);
                    });
                }
            }
        });
        self.open_popup(&popup, events, window, cx);
    }

    /// Opens the File Structure popup for the active editor. Asked for again while it is open,
    /// it shows or hides the members the file's types inherit.
    pub fn show_file_structure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.file_structure() {
            open.update(cx, |popup, cx| popup.toggle_show_inherited(cx));
            return;
        }
        let editor = self.editor().map(|editor| editor.read(cx));
        let file = editor
            .map(|editor| StructureFile { path: editor.path().to_string_lossy().into_owned(), doc: editor.doc() });
        let caret = editor.map(|editor| {
            let (line, col) = editor.caret_position();
            ide_core::Pos::new(line, col)
        });
        let label = self.label.clone();
        let popup = cx.new(|cx| FileStructure::new(&label, file, caret, cx));
        let events = cx.subscribe_in(&popup, window, |workbench, _, event: &FileStructureEvent, window, cx| {
            workbench.close_popup(window, cx);
            if let FileStructureEvent::Reveal { path, line, col } = event {
                workbench.open_project_file_at(path.clone(), *line, *col, window, cx);
            }
        });
        self.open_popup(&popup, events, window, cx);
    }

    pub fn file_structure(&self) -> Option<Entity<FileStructure>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    pub fn search_everywhere(&self) -> Option<Entity<SearchEverywhere>> {
        self.popup.as_ref().and_then(|popup| popup.view.clone().downcast().ok())
    }

    /// Moves the caret of the active editor to where its file was asked to be shown.
    fn serve_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor().cloned() else { return };
        let path = editor.read(cx).path().to_string_lossy().into_owned();
        let Content::Project { reveals, .. } = &mut self.content else { return };
        if let Some((line, col)) = reveals.remove(&path) {
            editor.update(cx, |editor, cx| {
                let at = editor.offset_of(line, col);
                editor.select(at, at, cx);
                editor.scroll_to_line(line.saturating_sub(5), cx);
            });
        }
    }

    /// Brings back the tab that was closed last, where it was.
    pub fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Project { tabs, .. } = &mut self.content else { return };
        let Some(reopened) = tabs.reopen_closed() else { return };
        let path = tabs
            .pane(&reopened.pane_id)
            .and_then(|pane| pane.tab(&reopened.tab_id))
            .filter(|tab| tab.input.kind == tab_views::FILE_KIND)
            .and_then(|tab| tab.input.path.clone());
        self.tabs_changed(true, window, cx);
        if let Some(path) = path {
            self.load_tab(reopened.tab_id, PathBuf::from(path), true, window, cx);
        }
    }

    /// Opens the document of the active tab if it has none yet: a tab restored with the project
    /// is only a title until it is shown.
    fn load_active_tab(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Project { tabs, editors, loading, .. } = &self.content else { return };
        // Every pane shows its selected tab; only the active pane's takes the keyboard.
        let mut documents: Vec<(String, PathBuf, bool)> = Vec::new();
        let mut views: Vec<(String, EditorInput)> = Vec::new();
        for pane in &tabs.panes {
            let Some(tab) = pane.active_tab() else { continue };
            let focus = focus && pane.id == tabs.active_pane;
            if tab.input.kind == tab_views::FILE_KIND {
                if editors.contains_key(&tab.id) || loading.contains(&tab.id) {
                    continue;
                }
                if let Some(path) = tab.input.path.clone() {
                    documents.push((tab.id.clone(), PathBuf::from(path), focus));
                }
            } else if tab_views::kind(&tab.input.kind).is_some() && self.tab_view(&tab.id).is_none() {
                views.push((tab.id.clone(), tab.input.clone()));
            }
        }
        for (tab, path, focus) in documents {
            self.load_tab(tab, path, focus, window, cx);
        }
        // A view takes the keyboard in `tabs_changed`, which calls this.
        for (tab, input) in views {
            self.build_view_tab(tab, input, window, cx);
        }
    }

    /// The editor of `tab` took the keyboard: its pane is the active one.
    fn editor_took_focus(&mut self, tab: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Project { tabs, .. } = &mut self.content else { return };
        let Some(pane) = tabs.panes.iter().find(|pane| pane.tab(tab).is_some()).map(|pane| pane.id.clone()) else {
            return;
        };
        if tabs.active_pane != pane && tabs.set_active_pane(&pane) {
            // The keyboard is already where it belongs.
            self.tabs_changed(false, window, cx);
            cx.notify();
        }
    }

    /// Splits the active pane: a second view of its file opens beside (or below) it, at the same
    /// place, and takes the keyboard.
    pub fn split_editor(&mut self, dir: SplitDir, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.editor().map(|editor| {
            let editor = editor.read(cx);
            (editor.path().to_string_lossy().into_owned(), editor.caret_position())
        });
        let Content::Project { tabs, reveals, .. } = &mut self.content else { return };
        let pane = tabs.active_pane.clone();
        if tabs.split_pane(&pane, dir).is_none() {
            return;
        }
        if let Some((path, caret)) = at {
            reveals.insert(path, caret);
        }
        self.tabs_changed(true, window, cx);
    }

    /// Moves the active tab to the neighbouring pane, or into a new pane on the right when there
    /// is none.
    pub fn move_tab_to_opposite_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_tabs(window, cx, |tabs, pane| {
            let Some(tab) = tabs.pane(pane).and_then(|pane| pane.active.clone()) else { return false };
            match tabs.neighbour_pane(pane).filter(|other| *other != pane).map(str::to_owned) {
                Some(target) => {
                    let end = tabs.pane(&target).map_or(0, |pane| pane.tabs.len());
                    tabs.move_tab(pane, &tab, &target, end, None) | tabs.set_active_pane(&target)
                }
                None => tabs.move_tab_to_new_split(pane, &tab, pane, SplitEdge::Right).is_some(),
            }
        });
    }

    /// Opens the active tab's file in the neighbouring pane as well, or splits when there is none.
    pub fn open_in_opposite_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tabs) = self.tabs() else { return };
        let pane = tabs.active_pane.clone();
        let Some(input) = tabs.pane(&pane).and_then(|pane| pane.active_tab()).map(|tab| tab.input.clone()) else {
            return;
        };
        match tabs.neighbour_pane(&pane).filter(|other| *other != pane).map(str::to_owned) {
            None => self.split_editor(SplitDir::Row, window, cx),
            Some(target) => self.with_tabs(window, cx, |tabs, _| {
                tabs.open_tab(&target, input, OpenTabOptions::default()).is_some() | tabs.set_active_pane(&target)
            }),
        }
    }

    /// Gives the keyboard to the next (or previous) pane that shows a tab, going round.
    pub fn focus_splitter(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.with_tabs(window, cx, |tabs, pane| {
            let shown: Vec<String> = tabs
                .pane_ids()
                .into_iter()
                .filter(|id| tabs.pane(id).is_some_and(|pane| pane.active.is_some()))
                .map(str::to_owned)
                .collect();
            let Some(at) = shown.iter().position(|id| id == pane).filter(|_| shown.len() > 1) else { return false };
            let next = if forward { (at + 1) % shown.len() } else { (at + shown.len() - 1) % shown.len() };
            tabs.set_active_pane(&shown[next])
        });
    }

    /// Stores the project's editor session now: its tabs and splits, and where each file's
    /// editor is; a file not opened since the session was restored keeps what was saved for it.
    pub fn save_session(&mut self, cx: &mut App) {
        self.session_save = None;
        let Content::Project { tabs, editors, restored_views, layout, .. } = &self.content else { return };
        let mut files = restored_views.clone();
        for editor in editors.values() {
            let editor = editor.read(cx);
            let view =
                FileViewState { anchor: editor.anchor(), head: editor.caret(), top_line: Some(editor.top_line(cx)) };
            files.insert(editor.path().to_string_lossy().into_owned(), view);
        }
        let saved = session::serialize_session(tabs, &files, tab_views::restorable);
        let root = layout.scope().to_owned();
        if cx.has_global::<UiStorage>() {
            session::store_session(cx.global_mut::<UiStorage>(), &root, saved.as_ref());
        }
    }

    /// Opens the document of tab `tab_id` in the backend and gives the tab its editor.
    /// With `focus` the editor takes the keyboard once it is there; a tab restored with the
    /// project loads without taking it from wherever it is.
    fn load_tab(&mut self, tab_id: String, path: PathBuf, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.load_tab_probed(tab_id, path, focus, false, window, cx);
    }

    /// [`Workbench::load_tab`]; with `probe` the file's content is looked at first, in the same
    /// backend call: binary or very large content turns the tab into the hex view, and no
    /// document is opened for it.
    fn load_tab_probed(
        &mut self,
        tab_id: String,
        path: PathBuf,
        focus: bool,
        probe: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One document per tab: a tab that has its editor, or is getting it, is left alone.
        let Content::Project { loading, editors, .. } = &mut self.content else { return };
        if editors.contains_key(&tab_id) || !loading.insert(tab_id.clone()) {
            return;
        }
        let label = self.label.clone();
        let opened = cx
            .global::<Backend>()
            .with_host(move |host| async move { tab_views::open_tab_file(&host, &label, path, probe).await });
        cx.spawn_in(window, async move |workbench, cx| {
            let result = opened.await;
            let _ = workbench.update_in(cx, |workbench, window, cx| {
                let host = cx.global::<Backend>().host.clone();
                let label = workbench.label.clone();
                let opened = result.map_err(|error| error.to_string()).and_then(|opened| opened.map_err(|e| e.message));
                let info = match opened {
                    Ok(tab_views::TabFile::Viewer(input)) => {
                        if let Content::Project { loading, .. } = &mut workbench.content {
                            loading.remove(&tab_id);
                        }
                        workbench.tab_shows_viewer(&tab_id, input);
                        workbench.tabs_changed(focus, window, cx);
                        return;
                    }
                    Ok(tab_views::TabFile::Text(info)) => Ok(info),
                    Err(message) => Err(message),
                };
                let text = info.and_then(|info| {
                    let project = host.project(&label).map_err(|error| error.message)?;
                    let (version, rope) = project.documents.rope_snapshot(info.id).map_err(|error| error.message)?;
                    Ok((info, version, rope))
                });
                let still_open =
                    workbench.tabs().is_some_and(|tabs| tabs.panes.iter().any(|pane| pane.tab(&tab_id).is_some()));
                if let Content::Project { loading, .. } = &mut workbench.content {
                    loading.remove(&tab_id);
                }
                match text {
                    Ok((info, version, rope)) => {
                        let editor = cx.new(|cx| Editor::with_text(host, &label, &info, version, rope, cx));
                        if still_open {
                            workbench.install_intelligence(&editor, window, cx);
                        }
                        // A tab closed while its file was opening has no use for the editor;
                        // dropping it releases the document.
                        if let (true, Content::Project { editors, restored_views, .. }) =
                            (still_open, &mut workbench.content)
                        {
                            // The file is where it was left when the project was last open.
                            let path = editor.read(cx).path().to_string_lossy().into_owned();
                            if let Some(view) = restored_views.remove(&path) {
                                editor.update(cx, |editor, cx| {
                                    editor.select(view.anchor, view.head, cx);
                                    if let Some(line) = view.top_line {
                                        editor.scroll_to_line(line, cx);
                                    }
                                });
                            }
                            let focused = tab_id.clone();
                            let took_focus =
                                cx.on_focus_in(&editor.focus_handle(cx), window, move |workbench, window, cx| {
                                    workbench.editor_took_focus(&focused, window, cx);
                                });
                            editors.insert(tab_id.clone(), editor.clone());
                            if let Content::Project { editor_focus, .. } = &mut workbench.content {
                                editor_focus.insert(tab_id.clone(), took_focus);
                            }
                            workbench.wrap_markdown(&tab_id, &editor, window, cx);
                        }
                        workbench.serve_reveal(cx);
                        if still_open {
                            workbench.watch_debug_editor(&editor, window, cx);
                            workbench.watch_plugin_gutter(&editor, window, cx);
                        }
                    }
                    Err(message) => {
                        tracing::error!("cannot open the file: {message}");
                        // A tab without a document would be a dead end: it goes away again.
                        if let Content::Project { tabs, .. } = &mut workbench.content {
                            let pane =
                                tabs.panes.iter().find(|pane| pane.tab(&tab_id).is_some()).map(|pane| pane.id.clone());
                            if let Some(pane) = pane {
                                tabs.close_tab(&pane, &tab_id);
                            }
                        }
                    }
                }
                workbench.tabs_changed(focus, window, cx);
            });
        })
        .detach();
    }

    /// The message shown instead of an editor, when opening failed.
    pub fn failure(&self) -> Option<&str> {
        match &self.content {
            Content::Failed(message) => Some(message),
            _ => None,
        }
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title(cx);
        if self.shown_title.as_deref() != Some(title.as_str()) {
            window.set_window_title(&title);
            self.shown_title = Some(title);
        }
        self.publish_context(window, cx);
        self.sync_chrome(window, cx);
        if window.is_window_active() {
            menu::schedule_sync(window, cx);
        }
        let message = |text: SharedString| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::color(cx, "fg-muted"))
                .child(text)
        };
        let body = match &self.content {
            Content::Empty => match &self.welcome {
                Some((welcome, _)) => welcome.clone().into_any_element(),
                None => message("".into()).into_any_element(),
            },
            Content::Opening(path) => message(format!("Opening {}…", path.display()).into()).into_any_element(),
            Content::Failed(error) => message(error.clone()).into_any_element(),
            Content::Editor(editor) => self.light_editor_element(editor),
            Content::Viewer(view) => view.view.clone().into_any_element(),
            Content::Project { layout, panels, .. } => {
                let area = self.tabs().map(|tabs| self.editor_area(&tabs.layout, window, cx));
                let gap = theme::length(cx, "island-gap");
                let island = || {
                    div()
                        .bg(theme::color(cx, "bg-island"))
                        .rounded(theme::length(cx, "island-radius"))
                        .overflow_hidden()
                };
                let button = theme::length(cx, "stripe-icon-button");
                let stripe_gap = theme::length(cx, "h-titlebar") - button;
                let stripe_button = |declared: &ToolWindow| {
                    let shown = layout.is_visible(&declared.id);
                    let color = theme::color(cx, if shown { "fg-emphasis" } else { "fg-muted" });
                    let action = format!("toolwindow.{}", declared.id);
                    let menu_id = declared.id.clone();
                    div()
                        .id(SharedString::from(format!("stripe:{}", declared.id)))
                        .debug_selector(|| format!("stripe:{}", declared.id))
                        .size(button)
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme::length(cx, "radius-toolbar-button"))
                        .when(shown, |el| el.bg(theme::color(cx, "bg-stripe-selected")))
                        .hover(|el| el.bg(theme::color(cx, "bg-stripe-hover")))
                        .child(theme::icon(cx, &declared.icon, 20.0, color))
                        .with_tooltip_shortcut(declared.title.clone(), action.clone())
                        .on_click(cx.listener(move |_, _, window, cx| {
                            execute_action_from(ActionSource::Toolbar, &action, None, window, cx);
                        }))
                        .on_mouse_down(
                            gpui_kit::MouseButton::Right,
                            cx.listener(move |workbench, event: &MouseDownEvent, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                workbench.show_stripe_button_menu(&menu_id, event.position, window, cx);
                            }),
                        )
                };
                // A stripe lists the windows docked on its side, then the split area's, and at
                // its foot those of the bottom: Bottom Left on the left stripe, Bottom Right on
                // the right one.
                let on_stripe = |slot: Slot| -> Vec<&ToolWindow> {
                    let showing = layout.visible().get(slot);
                    layout
                        .windows_in(slot)
                        .into_iter()
                        .filter(|w| layout.has_stripe_button(w) || showing == Some(w.id.as_str()))
                        .collect()
                };
                let stripe = |side: Anchor| {
                    let group = |slot: Slot| {
                        let windows = on_stripe(slot);
                        (!windows.is_empty()).then(|| {
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(stripe_gap)
                                .children(windows.into_iter().map(&stripe_button))
                        })
                    };
                    let lower = if side == Anchor::Left { Slot::Bottom } else { Slot::BottomSplit };
                    let outer = theme::length(cx, "stripe-outer-padding");
                    div()
                        .w(button + outer + gap)
                        .h_full()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(stripe_gap)
                        .when(side == Anchor::Left, |el| el.pl(outer).pr(gap))
                        .when(side == Anchor::Right, |el| el.pr(outer).pl(gap))
                        .children(group(Slot::new(side, false)))
                        .children(group(Slot::new(side, true)))
                        .child(div().flex_1())
                        .children(group(lower))
                };
                // A tool window: its title, a button to hide it, and its view.
                let tool_window = |id: &str| {
                    let (declared, panel) = (layout.window(id)?, panels.get(id)?);
                    let (hidden, options) = (id.to_owned(), id.to_owned());
                    Some(
                        island()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(theme::length(cx, "h-header"))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .pl(gpui_kit::px(12.0))
                                    .pr(gpui_kit::px(6.0))
                                    .text_color(theme::color(cx, "fg-strong"))
                                    .child(SharedString::from(declared.title.clone()))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("more:{id}")))
                                            .debug_selector(|| format!("more:{id}"))
                                            .with_tooltip("Options")
                                            .size(gpui_kit::px(22.0))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(gpui_kit::px(4.0))
                                            .hover(|el| el.bg(theme::color(cx, "bg-hover")))
                                            .child(theme::icon(cx, "more", 16.0, theme::color(cx, "fg-muted")))
                                            .on_mouse_down(
                                                gpui_kit::MouseButton::Left,
                                                cx.listener(move |workbench, event: &MouseDownEvent, window, cx| {
                                                    window.prevent_default();
                                                    cx.stop_propagation();
                                                    workbench.show_tool_window_options(
                                                        &options,
                                                        event.position,
                                                        window,
                                                        cx,
                                                    );
                                                }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("hide:{id}")))
                                            .debug_selector(|| format!("hide:{id}"))
                                            .with_tooltip_shortcut("Hide", "toolwindow.hideActive")
                                            .size(gpui_kit::px(22.0))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(gpui_kit::px(4.0))
                                            .hover(|el| el.bg(theme::color(cx, "bg-hover")))
                                            .child(theme::icon(cx, "minus", 16.0, theme::color(cx, "fg-muted")))
                                            .on_click(cx.listener(move |workbench, _, window, cx| {
                                                workbench.with_layout(window, cx, |layout| layout.hide(&hidden));
                                            })),
                                    ),
                            )
                            .child(div().flex_1().min_h_0().child(panel.view.clone())),
                    )
                };
                // A side: its primary and split areas, stacked on the left and right, side by
                // side at the bottom, sharing it by the stored ratio; a lone area takes it all.
                let side = |anchor: Anchor| {
                    let shown = |split: bool| layout.visible().get(Slot::new(anchor, split)).and_then(tool_window);
                    let (primary, split) = (shown(false), shown(true));
                    let both = primary.is_some() && split.is_some();
                    if primary.is_none() && split.is_none() {
                        return None;
                    }
                    let ratio = (layout.splits().get(anchor) as f32).clamp(0.0, 1.0);
                    let share = |area: gpui_kit::Div, part: f32| match (both, anchor) {
                        (false, _) => area.size_full(),
                        (true, Anchor::Bottom) => area.h_full().w(gpui_kit::relative(part)),
                        (true, _) => area.w_full().h(gpui_kit::relative(part)),
                    };
                    Some(
                        div()
                            .flex()
                            .gap(gap)
                            .when(anchor != Anchor::Bottom, |el| el.flex_col())
                            .children(primary.map(|area| share(area, ratio)))
                            .children(split.map(|area| share(area, 1.0 - ratio))),
                    )
                };
                let sizes = layout.sizes();
                // "Tool Window Bars" off hides the stripes, and so does Distraction Free Mode; the
                // windows stay reachable by action.
                let bars = self.shows_tool_window_bars(cx);
                let right_stripe = [Slot::Right, Slot::RightSplit, Slot::BottomSplit]
                    .into_iter()
                    .any(|slot| bars && !on_stripe(slot).is_empty())
                    .then(|| stripe(Anchor::Right));
                // The editor between the left and right sides; the bottom side spans below all three.
                let top = div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .children(
                        side(Anchor::Left).map(|s| s.w(gpui_kit::px(sizes.left as f32)).h_full().flex_none().mr(gap)),
                    )
                    .child(island().flex_1().h_full().min_w_0().flex().flex_col().children(area))
                    .children(
                        side(Anchor::Right).map(|s| s.w(gpui_kit::px(sizes.right as f32)).h_full().flex_none().ml(gap)),
                    );
                let bottom =
                    side(Anchor::Bottom).map(|s| s.h(gpui_kit::px(sizes.bottom as f32)).w_full().flex_none().mt(gap));
                // A maximized side covers the editor and the other sides.
                let centre = match layout.maximized().and_then(side) {
                    Some(maximized) => div().flex_1().min_w_0().h_full().child(maximized.size_full()),
                    None => div().flex_1().min_w_0().h_full().flex().flex_col().child(top).children(bottom),
                };
                div()
                    .size_full()
                    .flex()
                    // Without a stripe the islands keep the gap it would have given them.
                    .when(right_stripe.is_none(), |el| el.pr(gap))
                    .when(!bars, |el| el.pl(gap))
                    // Without a status bar the islands keep their distance from the window's edge.
                    .when(!self.shows_status_bar(cx), |el| el.pb(gap))
                    .children(bars.then(|| stripe(Anchor::Left)))
                    .child(centre)
                    .children(right_stripe)
                    .into_any_element()
            }
        };
        let framed = self.project_tree().is_some();
        div()
            .size_full()
            .bg(theme::color(cx, if framed { "bg-frame" } else { "bg-app" }))
            .text_color(theme::color(cx, "fg"))
            .text_size(theme::length(cx, "fs-ui"))
            // Modifier taps and mouse buttons have no application-level hook: forward them.
            .on_modifiers_changed(cx.listener(|workbench, event: &ModifiersChangedEvent, window, cx| {
                input::modifiers_changed(&event.modifiers, window, cx);
                let modifiers = event.modifiers;
                workbench.for_each_intelligence(cx, |intelligence, cx| intelligence.modifiers_changed(modifiers, cx));
            }))
            .capture_any_mouse_down(cx.listener(|_, event: &MouseDownEvent, window, cx| {
                if input::mouse_down(event.button, &event.modifiers, false, window, cx).consumed() {
                    cx.stop_propagation();
                }
            }))
            .flex()
            .flex_col()
            .child(self.title_bar().clone())
            .children(self.restricted_banner(cx))
            .child(div().flex_1().min_h_0().w_full().child(body))
            .children(self.plugin_gutter_layer(cx))
            .child(notification_balloons::layer(&self.balloons, cx))
            .children(self.merge_window_layer(cx))
            .children(self.diff_window_layer(cx))
            .children(self.edit_configs_layer())
            .children(self.debug_breakpoints_layer())
            .children(self.customize_toolbar_layer())
            .children(self.vcs_dialog_layer())
            .children(self.app_dialog_layer())
            .children(self.file_dialog_layer())
            .children(self.database_dialog_layer())
            .children(self.database_prompt_layer())
            .children(self.settings_layer(cx))
            .children(self.plugins_dialog_layer())
            .children(self.plugin_prompt_layer())
            .children(self.popup.as_ref().map(|popup| {
                // Over the whole window; a click beside the popup closes it.
                gpui_kit::deferred(
                    div()
                        .id("popup-backdrop")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .justify_center()
                        .pt(gpui_kit::px(80.0))
                        .on_mouse_down(
                            gpui_kit::MouseButton::Left,
                            cx.listener(|workbench, _, window, cx| {
                                // A pinned Find in Files stays when the click is beside it.
                                let pinned = workbench.find_popup().is_some_and(|popup| popup.read(cx).pinned());
                                if !pinned {
                                    workbench.close_popup(window, cx);
                                }
                            }),
                        )
                        .child(
                            div()
                                .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(popup.view.clone()),
                        ),
                )
                .with_priority(1)
            }))
            .children(self.shows_status_bar(cx).then(|| self.status_bar().clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AppDirs;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use std::time::Duration;

    mod project_threads;
    mod proofs3_context;
    mod proofs3_rename_lsp;
    mod proofs3_run;
    mod proofs3_windows;
    mod proofs4_run;
    mod proofs4_shell;
    mod proofs4_vcs;
    mod proofs5_database;
    mod proofs5_layout;
    mod proofs5_navigation;
    mod proofs5_run;
    mod proofs6_agents;
    mod proofs6_database;
    mod proofs6_workbench;
    mod proofs7_bookmark_digits;
    mod proofs7_quit;
    mod proofs8_build;
    mod proofs8_file_refactor;
    mod proofs9_log_users;
    mod proofs9_run_chooser;
    mod proofs_agents;
    mod proofs_database;
    mod proofs_editor_actions_find;
    mod proofs_editor_actions_markdown;
    mod proofs_editor_actions_navigate;
    mod proofs_editor_actions_run_console;
    mod proofs_editor_actions_tabs;
    mod proofs_editor_copy_reference;
    mod proofs_editor_search_everywhere;
    mod proofs_editor_search_find;
    mod proofs_editor_search_recent;
    mod proofs_editor_search_safe_delete;
    mod proofs_merge_apply;
    mod proofs_merge_change;
    mod proofs_merge_compare;
    mod proofs_merge_edit;
    mod proofs_merge_fixtures;
    mod proofs_merge_focus;
    mod proofs_services;
    mod proofs_tool_windows;
    mod proofs_vcs;
    mod smoke_backend_events;
    mod smoke_registered_actions;
    mod smoke_services;
    mod smoke_settings_pages;

    pub(super) fn start(cx: &mut TestAppContext) -> tempfile::TempDir {
        cx.executor().allow_parking();
        let home = tempfile::tempdir().expect("a temporary home");
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            input::init(cx);
            crate::editor::actions::register(cx);
            register_actions(cx);
            cx.set_global(UiStorage::default());
            Backend::init(AppDirs::under(home.path()), cx).expect("the backend starts");
            menu::init(cx);
            crate::terminal::test_support::isolate_shells(home.path(), cx);
        });
        // No Docker connection: showing Services looks up and runs no Docker program.
        let data = cx.update(|cx| cx.global::<Backend>().dirs.data.clone());
        std::fs::create_dir_all(&data).expect("the data folder is made");
        std::fs::write(data.join("docker.json"), r#"{ "connections": [], "registries": [] }"#)
            .expect("the Docker settings are written");
        home
    }

    /// View › Toggle Light/Dark Theme switches every window and stores the choice.
    #[gpui_kit::test]
    async fn the_theme_toggle_action_switches_and_stores_the_theme(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        cx.update(crate::theme::appearance::init);
        let (_workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        assert!(!cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
        cx.update(|window, cx| execute_action_or_throw("view.toggleTheme", None, window, cx)).expect("it runs");
        assert!(cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
        assert_eq!(cx.update(|_, cx| crate::platform::settings::global(cx).appearance.theme), "dark");
        cx.update(|window, cx| execute_action_or_throw("view.toggleTheme", None, window, cx)).expect("it runs");
        assert!(!cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
    }

    /// Waits for `done` while the backend's work comes back, without moving the executor's clock.
    /// Its budget is that of [`wait_long`]: on a loaded machine opening a project alone can take
    /// longer than three seconds.
    fn settle(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        let started = std::time::Instant::now();
        let poll = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            let reached = done(cx);
            if !reached {
                std::thread::sleep(Duration::from_millis(10));
            }
            reached
        };
        for _ in 0..3000 {
            if poll(cx) {
                return;
            }
        }
        // The state when the wait runs out, and whether more time would have been enough.
        let report = window_vcs_report(cx);
        let ran_out = started.elapsed();
        let later = (0..3000).find(|_| poll(cx)).map(|_| started.elapsed());
        panic!(
            "the backend did not deliver within thirty seconds (ran out after {ran_out:?}; reached later: {later:?})\n\
             state when it ran out: {report}"
        );
    }

    #[gpui_kit::test]
    async fn opens_a_loose_file_in_an_editor_and_types_into_it(cx: &mut TestAppContext) {
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let file = folder.path().join("todo.txt");
        std::fs::write(&file, "milk\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some() || w.failure().is_some()));
        let editor = workbench
            .read_with(cx, |w, _| w.editor().cloned())
            .unwrap_or_else(|| panic!("{:?}", workbench.read_with(cx, |w, _| w.failure().map(str::to_owned))));
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "milk\n");
        // The editor has focus: typing goes to the document without a click.
        cx.simulate_input("oat ");
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "oat milk\n");
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    /// ⌘S is `file.save` in the default keymap: the keystroke goes through the dispatcher, the
    /// registry and the editor to the backend, which writes the file.
    #[gpui_kit::test]
    async fn the_keymap_runs_editor_actions_on_the_windows_editor(cx: &mut TestAppContext) {
        use crate::platform::actions::{ActionSource, execute_action_from};
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let file = folder.path().join("todo.txt");
        std::fs::write(&file, "milk\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        // Without an editor the action has nothing to act on, and says so instead of failing silently.
        assert!(
            cx.update(|window, cx| crate::platform::actions::execute_action_or_throw("file.save", None, window, cx))
                .is_err()
        );
        workbench.update_in(cx, |workbench, window, cx| workbench.open_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");

        cx.simulate_input("oat ");
        cx.simulate_keystrokes("cmd-s");
        settle(cx, |_| std::fs::read_to_string(&file).is_ok_and(|text| text == "oat milk\n"));

        // The Edit menu's actions act on the same editor.
        assert!(cx.update(|window, cx| execute_action_from(ActionSource::Menu, "edit.undo", None, window, cx)));
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "milk\n");
        assert!(cx.update(|window, cx| execute_action_from(ActionSource::Menu, "edit.redo", None, window, cx)));
        assert!(cx.update(|window, cx| execute_action_from(ActionSource::Menu, "edit.selectAll", None, window, cx)));
        assert!(cx.update(|window, cx| execute_action_from(ActionSource::Menu, "edit.cut", None, window, cx)));
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "");
        assert!(cx.update(|window, cx| execute_action_from(ActionSource::Menu, "edit.paste", None, window, cx)));
        assert!(cx.update(|window, cx| execute_action_from(ActionSource::Menu, "edit.copy", None, window, cx)));
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "oat milk\n");
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn a_project_shows_its_tree_and_opens_files_in_the_editor(cx: &mut TestAppContext) {
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        std::fs::write(folder.path().join("main.zig"), "const a = 1;\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench
            .update_in(cx, |workbench, window, cx| workbench.open_project(folder.path().to_path_buf(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let tree = workbench.read_with(cx, |w, _| w.project_tree().cloned()).expect("a tree");
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.rows().len() == 2));
        // The tree has focus after a project opens: Down, Down, Enter opens the file.
        cx.simulate_keystrokes("down down enter");
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "const a = 1;\n");
        // The window is titled `<project> – <file>`.
        let root = folder.path().canonicalize().expect("the folder exists");
        let project_name = root.file_name().expect("a name").to_string_lossy().into_owned();
        assert_eq!(cx.window_title(), Some(format!("{project_name} – main.zig")));
        // The facts `when` clauses read follow what is open and focused.
        let truthy = |cx: &mut VisualTestContext, key: &str| {
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy(key))
        };
        for key in ["projectOpen", "textEditorActive", "editorFocus", "editorTextFocus"] {
            assert!(truthy(cx, key), "{key}");
        }
        for key in ["lightEdit", "editorHasSelection", "toolWindowFocus", "projectOpening", "editorReadOnly"] {
            assert!(!truthy(cx, key), "{key}");
        }
        let language =
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).get("editorLangId").cloned());
        assert_eq!(language, Some(crate::platform::context::ContextValue::from("zig")));
        cx.simulate_keystrokes("cmd-a");
        cx.run_until_parked();
        assert!(truthy(cx, "editorHasSelection"));
        cx.simulate_keystrokes("left");
        // The editor takes focus, so typing edits the file that was opened.
        cx.simulate_input("// ");
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "// const a = 1;\n");
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    // src/editor/core/view/statusItems.test.tsx (caret, line separator and encoding widgets)
    #[gpui_kit::test]
    async fn the_status_bar_follows_the_editors_caret_and_selection(cx: &mut TestAppContext) {
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let file = folder.path().join("notes.txt");
        std::fs::write(&file, "ab\r\ncd\r\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        assert!(workbench.read_with(cx, |w, cx| w.status_items(cx)).is_empty());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let items = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, cx| w.status_items(cx));
        assert_eq!(items(cx), ["1:1", "CRLF", "UTF-8"]);
        cx.simulate_keystrokes("down right");
        assert_eq!(items(cx), ["2:2", "CRLF", "UTF-8"]);
        cx.simulate_keystrokes("shift-right");
        assert_eq!(items(cx), ["2:3 (1 chars)", "CRLF", "UTF-8"]);
        // A line break is one character of the selection, whatever the file's line separator.
        cx.simulate_keystrokes("cmd-a");
        assert_eq!(items(cx)[0], "3:1 (6 chars, 2 line breaks)");
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    // src/workbench/toolwindows/store.test.ts › registers toolwindow.<id> and generic actions
    #[gpui_kit::test]
    async fn tool_window_actions_show_focus_and_hide_the_project_view(cx: &mut TestAppContext) {
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        std::fs::write(folder.path().join("main.zig"), "const a = 1;\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench
            .update_in(cx, |workbench, window, cx| workbench.open_project(folder.path().to_path_buf(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let tree = workbench.read_with(cx, |w, _| w.project_tree().cloned()).expect("a tree");
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.rows().len() == 2));
        let shown = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.tool_windows().expect("a layout").is_visible("project"))
        };
        let truthy = |cx: &mut VisualTestContext, key: &str| {
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy(key))
        };
        let tree_focused =
            |cx: &mut VisualTestContext| cx.update(|window, cx| tree.focus_handle(cx).is_focused(window));
        // A project opens with its Project view shown and focused.
        assert!(shown(cx) && tree_focused(cx));
        assert!(truthy(cx, "toolWindow.project.visible") && truthy(cx, "toolWindowFocus"));

        // ⌘1 on the focused window hides it; again shows and focuses it.
        cx.simulate_keystrokes("cmd-1");
        cx.run_until_parked();
        assert!(!shown(cx) && !tree_focused(cx));
        assert!(!truthy(cx, "toolWindow.project.visible") && !truthy(cx, "toolWindowFocus"));
        cx.simulate_keystrokes("cmd-1");
        cx.run_until_parked();
        assert!(shown(cx) && tree_focused(cx));

        // With an editor open, Escape in the tool window goes to the editor and ⌘1 comes back.
        cx.simulate_keystrokes("down down enter");
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.simulate_keystrokes("cmd-1");
        cx.run_until_parked();
        assert!(shown(cx) && tree_focused(cx));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(shown(cx) && !tree_focused(cx) && truthy(cx, "editorFocus"));
        // Hide All Tool Windows hides it; Jump to Last Tool Window (F12) brings it back, focused.
        cx.simulate_keystrokes("cmd-shift-f12");
        cx.run_until_parked();
        assert!(!shown(cx));
        cx.simulate_keystrokes("f12");
        cx.run_until_parked();
        assert!(shown(cx) && tree_focused(cx));

        // The layout is stored under the project folder's path as its key, in its JSON shape.
        let scope = folder.path().canonicalize().expect("the folder exists").to_string_lossy().into_owned();
        let stored =
            cx.update(|_, cx| cx.global::<UiStorage>().get(&storage_key(&scope)).map(str::to_owned)).expect("stored");
        let stored: serde_json::Value = serde_json::from_str(&stored).expect("JSON");
        assert_eq!(stored["v"], 2);
        assert_eq!(stored["visible"]["left"], "project");
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    // src/editor/core/model/model.test.ts (tabs), driven through the window and the keymap
    #[gpui_kit::test]
    async fn files_open_in_tabs_that_the_keymap_cycles_closes_and_reopens(cx: &mut TestAppContext) {
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::create_dir(root.join("a")).expect("a folder");
        std::fs::create_dir(root.join("b")).expect("a folder");
        for (file, text) in [("a/mod.rs", "// a\n"), ("b/mod.rs", "// b\n"), ("notes.txt", "notes\n")] {
            std::fs::write(root.join(file), text).expect("the file is written");
        }
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let open = |cx: &mut VisualTestContext, file: &str| {
            let path = root.join(file);
            workbench.update_in(cx, |workbench, window, cx| workbench.open_project_file(path, window, cx));
            let expected = if file == "notes.txt" {
                "notes\n"
            } else if file.starts_with('a') {
                "// a\n"
            } else {
                "// b\n"
            };
            settle(cx, |cx| workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).text() == expected)));
        };
        let titles = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.tab_titles());
        let active = |cx: &mut VisualTestContext| {
            titles(cx).into_iter().find(|title| title.active).map(|title| title.name).unwrap_or_default()
        };
        let tab = |name: &str, hint: Option<&str>, active: bool| TabTitle {
            name: name.to_owned(),
            hint: hint.map(str::to_owned),
            active,
        };
        let text = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, cx| w.editor().map(|e| e.read(cx).text()));

        open(cx, "a/mod.rs");
        assert_eq!(titles(cx), [tab("mod.rs", None, true)]);
        open(cx, "notes.txt");
        open(cx, "b/mod.rs");
        // Two files of the same name are told apart by their folders.
        assert_eq!(
            titles(cx),
            [tab("mod.rs", Some("a"), false), tab("notes.txt", None, false), tab("mod.rs", Some("b"), true)]
        );
        assert_eq!(text(cx).as_deref(), Some("// b\n"));
        // Opening a file that has a tab brings that tab to the front instead of adding one.
        open(cx, "notes.txt");
        assert_eq!((titles(cx).len(), active(cx)), (3, "notes.txt".to_owned()));

        // Ctrl+Right / Ctrl+Left cycle through the tabs, wrapping around.
        cx.simulate_keystrokes("ctrl-right");
        cx.run_until_parked();
        assert_eq!(text(cx).as_deref(), Some("// b\n"));
        cx.simulate_keystrokes("ctrl-right");
        cx.run_until_parked();
        assert_eq!(text(cx).as_deref(), Some("// a\n"));
        cx.simulate_keystrokes("ctrl-left ctrl-left");
        cx.run_until_parked();
        assert_eq!(active(cx), "notes.txt");

        // ⌘W closes the active tab, saving what was typed into it; the editor of a neighbour takes over.
        cx.simulate_input("more ");
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert_eq!(titles(cx).len(), 2);
        assert!(text(cx).is_some());
        settle(cx, |_| std::fs::read_to_string(root.join("notes.txt")).is_ok_and(|text| text == "more notes\n"));
        // Reopen Closed Tab brings it back where it was, with the saved text.
        cx.update(|window, cx| crate::platform::actions::execute_action("editor.reopenClosedTab", None, window, cx));
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).text() == "more notes\n"))
        });
        assert_eq!((titles(cx).len(), active(cx)), (3, "notes.txt".to_owned()));

        // Close Other Tabs, then closing the last one leaves an empty editor area.
        cx.update(|window, cx| crate::platform::actions::execute_action("editor.closeOtherTabs", None, window, cx));
        cx.run_until_parked();
        assert_eq!(titles(cx), [tab("notes.txt", None, true)]);
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert!(titles(cx).is_empty() && text(cx).is_none());
        let truthy =
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("editorTabsOpen"));
        assert!(!truthy);
        // Every closed tab released its document in the backend.
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        settle(cx, |_| host.project("w1").is_ok_and(|project| project.documents.open_documents().is_empty()));
        host.close_project("w1");
    }

    // src/workbench/Workbench.test.tsx › background tasks › tracks task events and shows the current task
    #[gpui_kit::test]
    async fn backend_tasks_and_notifications_reach_the_window(cx: &mut TestAppContext) {
        use ide_core::TaskId;
        let _home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        let send = |cx: &mut VisualTestContext, window: Option<&str>, event: IdeEvent| {
            let event = BackendEvent { window: window.map(str::to_owned), event };
            workbench.update(cx, |workbench, cx| workbench.backend_event(&event, cx));
        };
        let task = |done: bool, message: &str| IdeEvent::Task {
            id: TaskId(7),
            title: "Importing".to_owned(),
            message: Some(message.to_owned()),
            fraction: Some(0.5),
            done,
            cancellable: true,
        };
        let status = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.task_status());
        // Indexing is listed at once, with its counts; a second task is counted behind it.
        send(cx, Some("w1"), IdeEvent::IndexState { indexing: true, files: 1200, symbols: 34 });
        assert_eq!(status(cx).as_deref(), Some("Indexing: 1,200 files · 34 symbols"));
        // A short task is not shown unless it outlives the show delay; another window's is ignored.
        send(cx, Some("w1"), task(false, "module a"));
        send(cx, Some("other"), IdeEvent::IndexState { indexing: false, files: 0, symbols: 0 });
        assert_eq!(workbench.read_with(cx, |w, _| w.tasks().tasks().len()), 1);
        workbench.update(cx, |workbench, cx| workbench.tick_tasks(now_ms() + tasks::SHOW_DELAY_MS + 1.0, cx));
        assert_eq!(status(cx).as_deref(), Some("Indexing: 1,200 files · 34 symbols (+1)"));
        // Indexing ends and leaves after its linger; the import remains.
        send(cx, Some("w1"), IdeEvent::IndexState { indexing: false, files: 1200, symbols: 34 });
        workbench.update(cx, |workbench, cx| workbench.tick_tasks(now_ms() + tasks::DONE_LINGER_MS + 1.0, cx));
        assert_eq!(status(cx).as_deref(), Some("Importing: module a"));

        // A notification for every window (no window named) is kept in this one's list.
        send(
            cx,
            None,
            IdeEvent::Notify { level: NotifyLevel::Warning, title: "Disk is almost full".to_owned(), body: None },
        );
        let titles: Vec<String> =
            workbench.read_with(cx, |w, _| w.notifications().items().iter().map(|n| n.title.clone()).collect());
        assert_eq!(titles, ["Disk is almost full"]);
    }

    /// The title of the close or open question shown over the workbench's window, while one is.
    fn app_dialog_title(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<String> {
        workbench.read_with(cx, |w, cx| w.app_dialog().map(|dialog| dialog.read(cx).title().to_owned()))
    }

    /// The text of the window's editor.
    fn editor_text(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<String> {
        workbench.read_with(cx, |w, cx| w.editor().map(|editor| editor.read(cx).text()))
    }

    /// Closing the last project window with unsaved changes asks about them, with autosave on as by
    /// default: Cancel keeps the project and the text, Don't Save leaves the file as it is on disk,
    /// Save All writes it; each time the window stays and shows the Welcome screen. Nothing unsaved
    /// asks nothing.
    #[gpui_kit::test]
    async fn closing_asks_about_unsaved_changes_whatever_autosave_says(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("notes.txt", "notes\n")]);
        assert!(cx.update(|_, cx| crate::platform::settings::effective("w1", cx).files.auto_save), "autosave is on");
        let disk = || std::fs::read_to_string(root.join("notes.txt")).expect("readable");
        let reopen = |cx: &mut VisualTestContext| {
            workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
            settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
            workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
            settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
            cx.update(|window, cx| {
                let editor = workbench.read(cx).editor().cloned().expect("an editor");
                // A reopened tab comes back with its caret: typing starts at the top.
                editor.update(cx, |editor, cx| editor.select(0, 0, cx));
                window.focus(&editor.focus_handle(cx), cx);
            });
            cx.simulate_input("my ");
        };
        let welcomes = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.project_tree().is_none() && w.welcome().is_some())
        };

        // Nothing unsaved: the last project window goes back to the Welcome screen at once.
        assert!(!cx.simulate_close());
        cx.run_until_parked();
        assert!(cx.pending_prompt().is_none() && app_dialog_title(&workbench, cx).is_none());
        assert!(welcomes(cx));
        assert_eq!(cx.windows().len(), 1);

        reopen(cx);
        assert_eq!(workbench.read_with(cx, |w, cx| w.unsaved_count(cx)), 1);
        assert!(!cx.simulate_close());
        cx.run_until_parked();
        let (title, body) = cx.pending_prompt().expect("the question is asked");
        assert_eq!(title, "Unsaved Changes");
        let project = root.file_name().expect("a name").to_string_lossy().into_owned();
        assert_eq!(body, format!("1 file has unsaved changes in {project}. Save before closing?"));
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(editor_text(&workbench, cx).as_deref(), Some("my notes\n"));
        assert_eq!(disk(), "notes\n", "autosave saved nothing while the question was asked");
        assert!(workbench.read_with(cx, |w, _| w.project_tree().is_some()), "the project stays");

        // Don't Save: the window leaves the project, and the file keeps what it had on disk.
        assert!(!cx.simulate_close());
        cx.run_until_parked();
        cx.simulate_prompt_answer("Don't Save");
        settle(cx, welcomes);
        for _ in 0..20 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
        }
        assert_eq!(disk(), "notes\n", "Don't Save holds with autosave on");

        // Save All writes the file, then the window leaves the project.
        reopen(cx);
        assert!(!cx.simulate_close());
        cx.run_until_parked();
        cx.simulate_prompt_answer("Save All");
        settle(cx, |cx| welcomes(cx) && disk() == "my notes\n");
        assert_eq!(cx.windows().len(), 1);
    }

    /// Opening another project in this window first settles leaving the one it shows: the unsaved
    /// question comes after "Open Project" (This Window); Cancel keeps the project and the text,
    /// Don't Save opens the other one and leaves the file as it is on disk. Close Project with
    /// nothing unsaved asks nothing; an explicit `{path, newWindow: false}` opens here without the
    /// "Open Project" question.
    #[gpui_kit::test]
    async fn opening_another_project_here_asks_about_the_unsaved_changes_of_this_one(cx: &mut TestAppContext) {
        let _home = start(cx);
        let other = tempfile::tempdir().expect("a temporary folder");
        let other_root = other.path().canonicalize().expect("the folder exists");
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&other_root, true, false)).expect("trusted");
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("notes.txt", "notes\n")]);
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.update(|window, cx| {
            let editor = workbench.read(cx).editor().cloned().expect("an editor");
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.simulate_input("my ");
        let shown = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, cx| w.project_root(cx));
        let path = serde_json::Value::String(other_root.to_string_lossy().into_owned());

        run_action(cx, "file.openProjectPath", Some(path.clone()));
        assert_eq!(app_dialog_title(&workbench, cx).as_deref(), Some("Open Project"));
        click_on(cx, "prompt-button-same");
        let (title, _) = cx.pending_prompt().expect("leaving asks about the unsaved file");
        assert_eq!(title, "Unsaved Changes");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(shown(cx).as_deref(), Some(&*root), "Cancel keeps the project");
        assert_eq!(editor_text(&workbench, cx).as_deref(), Some("my notes\n"));

        run_action(cx, "file.openProjectPath", Some(path));
        click_on(cx, "prompt-button-same");
        cx.simulate_prompt_answer("Don't Save");
        settle(cx, |cx| shown(cx).as_deref() == Some(&*other_root));
        assert_eq!(std::fs::read_to_string(root.join("notes.txt")).expect("readable"), "notes\n");

        // Close Project with nothing unsaved goes to the Welcome screen without a question.
        run_action(cx, "file.closeProject", None);
        assert!(cx.pending_prompt().is_none() && app_dialog_title(&workbench, cx).is_none());
        assert!(workbench.read_with(cx, |w, _| w.project_tree().is_none() && w.welcome().is_some()));

        // An explicit request names the window: no "Open Project" question.
        let explicit = serde_json::json!({ "path": root.to_string_lossy(), "newWindow": false });
        run_action(cx, "file.openProjectPath", Some(explicit));
        assert!(app_dialog_title(&workbench, cx).is_none());
        settle(cx, |cx| shown(cx).as_deref() == Some(&*root));
        assert_eq!(cx.windows().len(), 1);
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// Closing the last window, which shows no project, confirms the exit (`general.confirmExit`,
    /// on by default): Cancel keeps the window; Exit with "Don't ask again" closes it and turns the
    /// setting off.
    #[gpui_kit::test]
    async fn closing_the_last_window_confirms_the_exit_and_dont_ask_again_turns_it_off(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        assert!(cx.update(|_, cx| crate::platform::settings::global(cx).general.confirm_exit));
        assert!(!cx.simulate_close());
        cx.run_until_parked();
        assert_eq!(app_dialog_title(&workbench, cx).as_deref(), Some("Confirm Exit"));
        let body = cx.update(|_, cx| {
            let i18n = &cx.default_global::<crate::platform::i18n::Localization>().0;
            i18n.t("app.exit.body", &[])
        });
        assert_eq!(body, "Are you sure you want to exit OpenIDEA?");
        click_on(cx, "prompt-button-cancel");
        assert!(app_dialog_title(&workbench, cx).is_none());
        assert_eq!(cx.windows().len(), 1, "Cancel keeps the window");
        assert!(cx.update(|_, cx| crate::platform::settings::global(cx).general.confirm_exit));

        assert!(!cx.simulate_close());
        cx.run_until_parked();
        assert_eq!(app_dialog_title(&workbench, cx).as_deref(), Some("Confirm Exit"));
        click_on(cx, "prompt-checkbox");
        click_on(cx, "prompt-button-exit");
        settle(cx, |cx| cx.windows().is_empty());
        assert!(
            !cx.read(|cx| crate::platform::settings::global(cx).general.confirm_exit),
            "Don't ask again turns it off"
        );
    }

    /// Closing a project window whose run configuration still runs asks to terminate it first
    /// (`run.confirmTerminate`), then about the unsaved documents: cancelling either keeps the
    /// process running and the project open; Terminate then Don't Save stops it and the window goes
    /// back to the Welcome screen once it has ended.
    #[gpui_kit::test]
    async fn closing_with_a_running_process_asks_to_terminate_it_before_the_unsaved_question(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root, store) = run_project(cx, vec![program("nap", "/bin/sleep", "30")]);
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        let session = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| store.sessions.iter().find(|s| s.config.name == "nap").cloned())
        };
        run_action(cx, "run.run", None);
        wait_long(cx, "the start of sleep", |cx| session(cx).is_some_and(|s| s.is_live()));

        // Only the process: Cancel keeps it running and the project open.
        assert!(!cx.simulate_close());
        cx.run_until_parked();
        assert_eq!(app_dialog_title(&workbench, cx).as_deref(), Some("Process 'nap' Is Running"));
        click_on(cx, "prompt-button-cancel");
        assert!(session(cx).is_some_and(|s| s.is_live()));
        assert!(workbench.read_with(cx, |w, _| w.project_tree().is_some()));

        // The process and an unsaved file: Terminate, then Cancel on the unsaved question, keeps both.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.update(|window, cx| {
            let editor = workbench.read(cx).editor().cloned().expect("an editor");
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.simulate_input("my ");
        assert!(!cx.simulate_close());
        cx.run_until_parked();
        assert_eq!(app_dialog_title(&workbench, cx).as_deref(), Some("Process 'nap' Is Running"));
        click_on(cx, "prompt-button-terminate");
        let (title, _) = cx.pending_prompt().expect("the unsaved question follows");
        assert_eq!(title, "Unsaved Changes");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(session(cx).is_some_and(|s| s.is_live()), "cancelling the second question stops nothing");
        assert_eq!(editor_text(&workbench, cx).as_deref(), Some("my notes\n"));

        assert!(!cx.simulate_close());
        cx.run_until_parked();
        click_on(cx, "prompt-button-terminate");
        cx.simulate_prompt_answer("Don't Save");
        wait_long(cx, "the end of sleep", |cx| session(cx).is_none_or(|s| !s.is_live()));
        wait_long(cx, "the Welcome screen", |cx| workbench.read_with(cx, |w, _| w.welcome().is_some()));
        assert_eq!(std::fs::read_to_string(root.join("notes.txt")).expect("readable"), "notes\n");
    }

    /// Quit run as an action in a window with an unsaved editor confirms the exit, then asks about
    /// the unsaved documents of that same window: Cancel keeps the window and the text, and a later
    /// Quit asks again. The window cannot be updated from inside its own update, which is why the
    /// windows are asked from outside the action.
    #[gpui_kit::test]
    async fn quit_from_a_window_asks_about_its_unsaved_changes_and_cancel_keeps_it(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("notes.txt", "notes\n")]);
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.update(|window, cx| {
            let editor = workbench.read(cx).editor().cloned().expect("an editor");
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.simulate_input("my ");
        let reentrant = cx.update(|window, cx| window.window_handle().update(cx, |_, _, _| ()).is_err());
        assert!(reentrant, "a window's own update cannot reach it again");

        for _ in 0..2 {
            run_action(cx, "app.quit", None);
            assert_eq!(app_dialog_title(&workbench, cx).as_deref(), Some("Confirm Exit"));
            click_on(cx, "prompt-button-exit");
            let (title, _) = cx.pending_prompt().expect("the unsaved question is asked");
            assert_eq!(title, "Unsaved Changes");
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            assert_eq!(cx.windows().len(), 1, "the window stays");
            assert_eq!(editor_text(&workbench, cx).as_deref(), Some("my notes\n"));
            assert_eq!(std::fs::read_to_string(root.join("notes.txt")).expect("readable"), "notes\n");
        }
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// A tab whose document cannot be saved comes back with its text, and the failure is reported.
    #[cfg(unix)]
    #[gpui_kit::test]
    async fn a_tab_that_cannot_be_saved_reopens_with_its_text(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::create_dir(root.join("locked")).expect("a folder");
        let file = root.join("locked/notes.txt");
        std::fs::write(&file, "notes\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.simulate_input("my ");
        // Neither the file nor its folder can be written any more.
        let lock = |mode: u32, path: &std::path::Path| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("permissions change")
        };
        lock(0o444, &file);
        lock(0o555, &root.join("locked"));

        cx.simulate_keystrokes("cmd-w");
        settle(cx, |cx| workbench.read_with(cx, |w, _| !w.notifications().items().is_empty()));
        workbench.read_with(cx, |w, cx| {
            assert_eq!(w.notifications().items()[0].title, "Could not save all files");
            assert_eq!(w.tab_titles().len(), 1, "the tab is back");
            assert_eq!(w.editor().map(|e| e.read(cx).text()).as_deref(), Some("my notes\n"));
            assert_eq!(w.unsaved_count(cx), 1);
        });
        assert_eq!(std::fs::read_to_string(&file).expect("readable"), "notes\n");

        // Once the file can be written again, saving succeeds and nothing was lost.
        lock(0o755, &root.join("locked"));
        lock(0o644, &file);
        cx.simulate_keystrokes("cmd-s");
        settle(cx, |_| std::fs::read_to_string(&file).is_ok_and(|text| text == "my notes\n"));
        // The close wrote the document once: one failure, reported with the tab that came back.
        let failures = workbench.read_with(cx, |w, _| {
            w.notifications().items().iter().filter(|n| n.title == "Could not save all files").count()
        });
        assert_eq!(failures, 1, "{:?}", notification_titles(&workbench, cx));
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    // src/features/app/title.ts
    /// With Files › Autosave off nothing is saved behind the user's back: leaving a tab keeps
    /// it unsaved, and closing it asks. Cancel brings the tab back, Don't Save drops the edits,
    /// Save All writes them.
    #[gpui_kit::test]
    async fn without_autosave_closing_an_unsaved_tab_asks(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use ide_workspace::settings::SettingsScope;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("a.txt"), "a\n").expect("the file is written");
        std::fs::write(root.join("b.txt"), "b\n").expect("the file is written");
        cx.update(|cx| {
            crate::platform::settings::set("w1", SettingsScope::Global, "files.autoSave", &false.into(), cx)
                .expect("the setting is stored");
        });
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let open = |name: &str, cx: &mut VisualTestContext| {
            let path = root.join(name);
            workbench.update_in(cx, |workbench, window, cx| workbench.open_project_file(path.clone(), window, cx));
            settle(cx, |cx| {
                workbench.read_with(cx, |w, cx| w.editor().is_some_and(|editor| editor.read(cx).path() == path))
            });
        };
        let on_disk = |name: &str| std::fs::read_to_string(root.join(name)).expect("readable");
        let close = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| execute_action_or_throw("editor.closeTab", None, window, cx)).expect("it runs");
        };
        let names = |cx: &mut VisualTestContext| -> Vec<String> {
            workbench.read_with(cx, |w, _| w.tab_titles().into_iter().map(|tab| tab.name).collect())
        };

        open("a.txt", cx);
        cx.simulate_input("my ");
        // Leaving the tab does not save it.
        open("b.txt", cx);
        cx.run_until_parked();
        assert_eq!(on_disk("a.txt"), "a\n");
        assert_eq!(workbench.read_with(cx, |w, cx| w.unsaved_count(cx)), 1);

        // Closing it asks; Cancel brings the tab back with its text.
        open("a.txt", cx);
        close(cx);
        let (title, body) = cx.pending_prompt().expect("the question is asked");
        assert_eq!(title, "Unsaved Changes");
        let path = root.join("a.txt").to_string_lossy().into_owned();
        assert_eq!(body, format!("1 file has unsaved changes in {path}. Save before closing?"));
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(names(cx), ["b.txt", "a.txt"]);
        assert_eq!(workbench.read_with(cx, |w, cx| w.editor().map(|e| e.read(cx).text())).as_deref(), Some("my a\n"));
        assert_eq!(on_disk("a.txt"), "a\n");

        // Save All writes it and the tab stays closed.
        close(cx);
        cx.simulate_prompt_answer("Save All");
        settle(cx, |_| on_disk("a.txt") == "my a\n");
        assert_eq!(names(cx), ["b.txt"]);

        // Don't Save drops the edits.
        cx.simulate_input("their ");
        close(cx);
        cx.simulate_prompt_answer("Don't Save");
        cx.run_until_parked();
        assert!(names(cx).is_empty());
        assert_eq!(on_disk("b.txt"), "b\n");
        assert_eq!(workbench.read_with(cx, |w, cx| w.unsaved_count(cx)), 0);
    }

    /// The Terminal docks at the bottom: its action shows it with a shell of the project's
    /// folder and the focus in it, typing reaches the shell, its stripe button is at the foot
    /// of the left stripe, and hiding it keeps the shell running for the next time.
    #[gpui_kit::test]
    async fn the_terminal_tool_window_runs_a_shell_at_the_bottom(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::terminal::view::TerminalView;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the folder is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        workbench.read_with(cx, |workbench, _| {
            let Content::Project { layout, .. } = &workbench.content else { panic!("a project is open") };
            assert_eq!(layout.slot_of(TERMINAL_TOOL_WINDOW), Some(Slot::Bottom));
            assert!(!workbench.is_tool_window_visible(TERMINAL_TOOL_WINDOW));
            assert!(workbench.tool_window_view(TERMINAL_TOOL_WINDOW).is_none(), "no shell before it is shown");
        });

        cx.update(|window, cx| execute_action_or_throw("toolwindow.terminal", None, window, cx)).expect("it runs");
        let terminals = |cx: &mut VisualTestContext| {
            workbench
                .read_with(cx, |w, _| w.tool_window_view(TERMINAL_TOOL_WINDOW))
                .and_then(|view| view.downcast::<crate::terminal::panel::TerminalPanel>().ok())
                .expect("the Terminal window holds its tabs")
        };
        let terminal: Entity<TerminalView> =
            terminals(cx).read_with(cx, |panel, _| panel.active_view().cloned()).expect("a first tab");
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(TERMINAL_TOOL_WINDOW)));
        assert_eq!(terminal.read_with(cx, |terminal, _| terminal.error().map(str::to_owned)), None);
        let long = |cx: &mut VisualTestContext, done: &dyn Fn(&mut VisualTestContext) -> bool| {
            for _ in 0..3000 {
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("the shell did not answer within thirty seconds");
        };
        let screen = |cx: &mut VisualTestContext| terminal.read_with(cx, |t, _| t.emulator().lines().join("\n"));
        // The focus is in the terminal: typing goes to the shell, which starts in the project.
        cx.simulate_input("echo in-$(basename \"$PWD\")-$((6*7))");
        cx.simulate_keystrokes("enter");
        let expected = format!("in-{}-42", root.file_name().expect("a name").to_string_lossy());
        long(cx, &|cx| screen(cx).contains(&expected));
        assert!(cx.update(|window, cx| {
            crate::platform::context::window_context(window, cx).is_truthy("toolWindowFocus")
        }));

        // The same action hides the focused window; the shell stays, and shows again as it was.
        cx.update(|window, cx| execute_action_or_throw("toolwindow.terminal", None, window, cx)).expect("it runs");
        assert!(!workbench.read_with(cx, |w, _| w.is_tool_window_visible(TERMINAL_TOOL_WINDOW)));
        assert!(terminal.read_with(cx, |terminal, _| terminal.id().is_some()), "hidden, not ended");
        cx.update(|window, cx| execute_action_or_throw("toolwindow.terminal", None, window, cx)).expect("it runs");
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(TERMINAL_TOOL_WINDOW)));
        assert!(screen(cx).contains(&expected));
        let again = terminals(cx).read_with(cx, |panel, _| panel.active_view().cloned()).expect("a tab");
        assert_eq!(again.entity_id(), terminal.entity_id(), "the same session");

        // New Terminal Tab opens a second shell with the keyboard in it; Close Terminal Session
        // ends it once the backend says the shell runs nothing, and the first tab shows again.
        cx.update(|window, cx| execute_action_or_throw("terminal.new", None, window, cx)).expect("it runs");
        let titles = |cx: &mut VisualTestContext| terminals(cx).read_with(cx, |panel, _| panel.tab_titles());
        assert_eq!(titles(cx), [("Local".to_owned(), false), ("Local (2)".to_owned(), true)]);
        cx.run_until_parked();
        let second = terminals(cx).read_with(cx, |panel, _| panel.active_view().cloned()).expect("a tab");
        assert!(cx.update(|window, cx| second.focus_handle(cx).is_focused(window)));
        cx.update(|window, cx| execute_action_or_throw("terminal.closeSession", None, window, cx)).expect("it runs");
        long(cx, &|cx| titles(cx).len() == 1);
        assert_eq!(titles(cx), [("Local".to_owned(), true)]);
        // Clear Terminal Buffer empties the first tab's screen down to its prompt.
        cx.update(|window, cx| {
            let focus = terminal.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| execute_action_or_throw("terminal.clear", None, window, cx)).expect("it runs");
        assert!(!screen(cx).contains(&expected));

        // The shell ends: its tab closes, and with the last tab the tool window hides. Shown
        // again, it starts a new shell.
        cx.simulate_input("exit");
        cx.simulate_keystrokes("enter");
        long(cx, &|cx| !workbench.read_with(cx, |w, _| w.is_tool_window_visible(TERMINAL_TOOL_WINDOW)));
        cx.update(|window, cx| execute_action_or_throw("toolwindow.terminal", None, window, cx)).expect("it runs");
        let fresh = terminals(cx).read_with(cx, |panel, _| panel.active_view().cloned()).expect("a new tab");
        assert_ne!(fresh.entity_id(), terminal.entity_id());
        assert_eq!(titles(cx).len(), 1);

        // With no tab left, New Terminal Tab opens exactly one and shows the window.
        cx.update(|window, cx| execute_action_or_throw("terminal.closeSession", None, window, cx)).expect("it runs");
        long(cx, &|cx| !workbench.read_with(cx, |w, _| w.is_tool_window_visible(TERMINAL_TOOL_WINDOW)));
        cx.update(|window, cx| execute_action_or_throw("terminal.new", None, window, cx)).expect("it runs");
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(TERMINAL_TOOL_WINDOW)));
        assert_eq!(titles(cx), [("Local".to_owned(), true)]);
    }

    /// New Terminal Tab on a project whose Terminal was never opened shows it with one tab,
    /// the keyboard in it.
    #[gpui_kit::test]
    async fn a_new_terminal_tab_opens_the_terminal_with_one_tab(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::terminal::panel::TerminalPanel;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the folder is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        cx.update(|window, cx| execute_action_or_throw("terminal.new", None, window, cx)).expect("it runs");
        let terminals = workbench
            .read_with(cx, |w, _| w.tool_window_view(TERMINAL_TOOL_WINDOW))
            .and_then(|view| view.downcast::<TerminalPanel>().ok())
            .expect("the Terminal window holds its tabs");
        assert_eq!(terminals.read_with(cx, |panel, _| panel.tab_titles()), [("Local".to_owned(), true)]);
        cx.run_until_parked();
        let shell = terminals.read_with(cx, |panel, _| panel.active_view().cloned()).expect("a tab");
        assert!(cx.update(|window, cx| shell.focus_handle(cx).is_focused(window)));
    }

    /// Blaze and Deep mode: the toggle writes `ide.mode` for every project and the backend
    /// applies it to this one; a project that names the mode itself is switched too.
    #[gpui_kit::test]
    async fn the_ide_mode_toggle_switches_between_blaze_and_deep(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::platform::settings;
        use ide_core::settings::IdeMode;
        use ide_workspace::settings::SettingsScope;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let toggle = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| execute_action_or_throw("ide.toggleMode", None, window, cx)).expect("it runs");
        };
        let backend_mode = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| cx.global::<Backend>().host.project("w1").map(|p| p.ide_mode()))
        };
        let toggled = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let action = cx.global::<ActionRegistry>().get("ide.toggleMode").expect("registered");
                action.is_toggled(window, cx)
            })
        };
        assert_eq!(workbench.read_with(cx, |w, cx| w.ide_mode(cx)), IdeMode::Light);
        assert_eq!(toggled(cx), Some(false));

        toggle(cx);
        assert_eq!(workbench.read_with(cx, |w, cx| w.ide_mode(cx)), IdeMode::Full);
        assert_eq!(cx.update(|_, cx| settings::global(cx).ide.mode), IdeMode::Full);
        assert_eq!(backend_mode(cx).ok(), Some(IdeMode::Full));
        assert_eq!(toggled(cx), Some(true));
        toggle(cx);
        assert_eq!(backend_mode(cx).ok(), Some(IdeMode::Light));

        // The project says Blaze itself: the global switch alone would change nothing here.
        cx.update(|_, cx| {
            settings::set("w1", SettingsScope::Project, "ide.mode", &"light".into(), cx).expect("stored");
        });
        toggle(cx);
        assert_eq!(cx.update(|_, cx| settings::global(cx).ide.mode), IdeMode::Full);
        assert_eq!(workbench.read_with(cx, |w, cx| w.ide_mode(cx)), IdeMode::Full);
        assert_eq!(backend_mode(cx).ok(), Some(IdeMode::Full));
    }

    /// A project's tabs come back when it is opened again: the same files in the same order,
    /// the same one active with its selection and scroll position, and the others loaded only
    /// when they are shown.
    #[gpui_kit::test]
    async fn a_projects_tabs_and_carets_are_restored(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        let long: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.join(name), &long).expect("the file is written");
        }
        let scope = root.to_string_lossy().into_owned();
        let key = format!("{}{scope}", session::STORAGE_PREFIX);
        let (first, cx1) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        first.update_in(cx1, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx1, |cx| first.read_with(cx, |w, _| w.project_tree().is_some()));
        assert_eq!(cx1.update(|_, cx| cx.global::<UiStorage>().get(&key).map(str::to_owned)), None);
        for name in ["a.txt", "b.txt"] {
            let path = root.join(name);
            first.update_in(cx1, |workbench, window, cx| workbench.open_project_file(path.clone(), window, cx));
            settle(cx1, |cx| {
                first.read_with(cx, |w, cx| w.editor().is_some_and(|editor| editor.read(cx).path() == path))
            });
        }
        let editor = first.read_with(cx1, |w, _| w.editor().cloned()).expect("b.txt is showing");
        editor.update(cx1, |editor, cx| {
            editor.select(700, 706, cx);
            editor.scroll_to_line(90, cx);
        });
        // Leaving the window writes the session as it stands.
        first.update(cx1, |workbench, cx| workbench.save_session(cx));
        let stored: serde_json::Value = cx1
            .update(|_, cx| serde_json::from_str(cx.global::<UiStorage>().get(&key).expect("stored")))
            .expect("JSON");
        let b = root.join("b.txt").to_string_lossy().into_owned();
        assert_eq!(stored["layout"]["active"], 1);
        assert_eq!(stored["layout"]["tabs"][1]["path"], b.as_str());
        assert_eq!(stored["files"][b.as_str()], serde_json::json!({ "anchor": 700, "head": 706, "topLine": 90 }));
        cx1.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));

        let (second, cx2) = cx.add_window_view(|window, cx| Workbench::new("w2", window, cx));
        second.update_in(cx2, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx2, |cx| second.read_with(cx, |w, _| w.editor().is_some()));
        let names: Vec<(String, bool)> =
            second.read_with(cx2, |w, _| w.tab_titles().into_iter().map(|tab| (tab.name, tab.active)).collect());
        assert_eq!(names, [("a.txt".to_owned(), false), ("b.txt".to_owned(), true)]);
        let restored = second.read_with(cx2, |w, _| w.editor().cloned()).expect("an editor");
        // Restoring a tab does not take the keyboard: it is in the Project view, as on any open.
        cx2.run_until_parked();
        assert!(cx2.update(|window, cx| {
            let tree = second.read(cx).project_tree().expect("a tree").focus_handle(cx);
            tree.is_focused(window) && !restored.focus_handle(cx).is_focused(window)
        }));
        restored.read_with(cx2, |editor, cx| {
            assert_eq!(editor.path(), root.join("b.txt"));
            assert_eq!((editor.anchor(), editor.caret(), editor.top_line(cx)), (700, 706, 90));
        });
        // Only the tab that shows has a document; the other gets its own when it is shown.
        let open_documents = |cx: &mut VisualTestContext| {
            second.read_with(cx, |w, _| match &w.content {
                Content::Project { editors, .. } => editors.len(),
                _ => 0,
            })
        };
        assert_eq!(open_documents(cx2), 1);
        // Tab switching belongs to the editor: with the keyboard there, the previous tab shows.
        cx2.update(|window, cx| {
            let focus = restored.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx2.run_until_parked();
        cx2.update(|window, cx| execute_action_or_throw("editor.prevTab", None, window, cx)).expect("it runs");
        settle(cx2, |cx| {
            second.read_with(cx, |w, cx| w.editor().is_some_and(|editor| editor.read(cx).path() == root.join("a.txt")))
        });
        assert_eq!(open_documents(cx2), 2);

        // Closing every tab leaves no session behind once the change has rested.
        cx2.update(|window, cx| execute_action_or_throw("editor.closeAllTabs", None, window, cx)).expect("it runs");
        cx2.executor().advance_clock(SESSION_SAVE_DELAY);
        cx2.run_until_parked();
        assert_eq!(cx2.update(|_, cx| cx.global::<UiStorage>().get(&key).map(str::to_owned)), None);
    }

    /// The Git tool window docks at the bottom and holds the project's log, read when the
    /// window is first shown.
    #[gpui_kit::test]
    async fn the_git_tool_window_shows_the_projects_log(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::vcs::log_view::GitLog;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs");
            assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "."]);
        git(&["-c", "user.name=Ada", "-c", "user.email=ada@example.test", "commit", "-q", "-m", "notes"]);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        assert!(
            workbench.read_with(cx, |w, _| w.tool_window_view(GIT_TOOL_WINDOW).is_none()),
            "not read before it shows"
        );
        cx.update(|window, cx| execute_action_or_throw("toolwindow.git", None, window, cx)).expect("it runs");
        let log = workbench
            .read_with(cx, |w, _| w.tool_window_view(GIT_TOOL_WINDOW))
            .and_then(|view| view.downcast::<GitLog>().ok())
            .expect("the Git window holds the log");
        workbench.read_with(cx, |workbench, _| {
            let Content::Project { layout, .. } = &workbench.content else { panic!("a project is open") };
            assert_eq!(layout.slot_of(GIT_TOOL_WINDOW), Some(Slot::Bottom));
            assert!(workbench.is_tool_window_visible(GIT_TOOL_WINDOW));
        });
        settle(cx, |cx| log.read_with(cx, |log, _| !log.is_loading()));
        let subjects: Vec<String> =
            log.read_with(cx, |log, _| log.commits().iter().map(|c| c.summary.clone()).collect());
        assert_eq!(subjects, ["notes"]);
        assert!(cx.update(|window, cx| log.focus_handle(cx).is_focused(window)), "showing it gives it the keyboard");

        // The Commit window shares the left side with the Project view: it takes its place, and
        // a changed file opens from it (F4, Jump to Source).
        std::fs::write(root.join("notes.txt"), "changed\n").expect("the file is written");
        cx.update(|window, cx| execute_action_or_throw("toolwindow.commit", None, window, cx)).expect("it runs");
        let changes = workbench
            .read_with(cx, |w, _| w.tool_window_view(COMMIT_TOOL_WINDOW))
            .and_then(|view| view.downcast::<crate::vcs::commit_view::CommitPanel>().ok())
            .expect("the Commit window holds the changes");
        workbench.read_with(cx, |workbench, _| {
            assert!(workbench.is_tool_window_visible(COMMIT_TOOL_WINDOW));
            assert!(!workbench.is_tool_window_visible(PROJECT_TOOL_WINDOW));
        });
        settle(cx, |cx| changes.read_with(cx, |changes, _| !changes.is_loading()));
        assert_eq!(changes.read_with(cx, |changes, _| changes.lines()), ["Changes (1)", "  notes.txt"]);
        cx.simulate_keystrokes("down down f4");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("notes.txt")))
        });
    }

    /// Show Diff on a change of the Commit tool window shows the diff in front of the window: HEAD's
    /// line on the left, the working copy's on the right, one difference. Next Difference makes it
    /// the current one, Compare Next File shows the group's other changed file, Jump to Source opens
    /// the file at the changed line and closes the diff, and Escape closes it.
    #[gpui_kit::test]
    async fn show_diff_on_a_change_of_the_commit_window_shows_and_walks_its_diff(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::vcs::commit_view::CommitPanel;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("the file is written");
        std::fs::write(root.join("b.txt"), "x\ny\nz\n").expect("the file is written");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs");
            assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "."]);
        git(&["-c", "user.name=Ada", "-c", "user.email=ada@example.test", "commit", "-q", "-m", "first"]);
        std::fs::write(root.join("a.txt"), "one\nTWO\nthree\n").expect("the file is written");
        std::fs::write(root.join("b.txt"), "x\nY\nz\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        cx.update(|window, cx| execute_action_or_throw("toolwindow.commit", None, window, cx)).expect("it runs");
        let changes = workbench
            .read_with(cx, |w, _| w.tool_window_view(COMMIT_TOOL_WINDOW))
            .and_then(|view| view.downcast::<CommitPanel>().ok())
            .expect("the Commit window holds the changes");
        settle(cx, |cx| changes.read_with(cx, |changes, _| changes.lines().len() == 3));
        assert_eq!(changes.read_with(cx, |changes, _| changes.lines()), ["Changes (2)", "  a.txt", "  b.txt"]);

        // Show Diff on the selected change.
        cx.simulate_keystrokes("down down");
        cx.update(|window, cx| execute_action_or_throw("vcs.showDiff", None, window, cx)).expect("it runs");
        let dialog = workbench.read_with(cx, |w, _| w.diff_dialog().cloned()).expect("the diff is shown");
        let viewer = dialog.read_with(cx, |dialog, _| dialog.viewer().clone());
        settle(cx, |cx| viewer.read_with(cx, |viewer, _| viewer.difference_count() == 1));
        let changed = |lines: Vec<crate::vcs_diff::view::PaneLine>| -> Vec<String> {
            lines.into_iter().filter(|line| line.kind.is_some()).map(|line| line.text).collect()
        };
        assert_eq!(changed(viewer.read_with(cx, |viewer, _| viewer.left_lines())), ["two"]);
        assert_eq!(changed(viewer.read_with(cx, |viewer, _| viewer.right_lines())), ["TWO"]);
        assert!(dialog.read_with(cx, |dialog, _| dialog.data().path.ends_with("a.txt")));

        // Next Difference makes the change the current one.
        assert_eq!(viewer.read_with(cx, |viewer, _| viewer.current_difference()), None);
        cx.update(|window, cx| execute_action_or_throw("diff.nextDifference", None, window, cx)).expect("it runs");
        assert_eq!(viewer.read_with(cx, |viewer, _| viewer.current_difference()), Some(0));

        // Compare Next File shows the other changed file of the group.
        cx.update(|window, cx| execute_action_or_throw("diff.nextFile", None, window, cx)).expect("it runs");
        settle(cx, |cx| {
            dialog.read_with(cx, |dialog, _| dialog.data().path.ends_with("b.txt"))
                && changed(viewer.read_with(cx, |viewer, _| viewer.right_lines())) == ["Y"]
        });
        assert_eq!(changed(viewer.read_with(cx, |viewer, _| viewer.left_lines())), ["y"]);

        // Jump to Source opens the file at the changed line and closes the diff.
        cx.update(|window, cx| execute_action_or_throw("diff.nextDifference", None, window, cx)).expect("it runs");
        cx.update(|window, cx| execute_action_or_throw("diff.jumpToSource", None, window, cx)).expect("it runs");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.diff_dialog().is_none()), "Jump to Source closes the diff");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("b.txt")))
        });
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("the file is open");
        settle(cx, |cx| editor.read_with(cx, |editor, _| editor.caret_position().0 == 1));

        // Escape closes the diff window.
        cx.update(|window, cx| execute_action_or_throw("vcs.showDiff", None, window, cx)).expect("it runs");
        assert!(workbench.read_with(cx, |w, _| w.diff_dialog().is_some()), "the active file's diff is shown");
        // The diff's sides are read off the main thread: the keys follow once they have arrived.
        settle(cx, |cx| context_is_on(cx, "diffEditorFocus") && context_is_on(cx, "vcsAvailable"));
        cx.simulate_keystrokes("escape");
        assert!(workbench.read_with(cx, |w, _| w.diff_dialog().is_none()), "Escape closes the diff");
        cx.run_until_parked();
        assert!(!context_is_on(cx, "diffEditorFocus"), "the diff window is gone");
    }

    /// Find in Files: the action shows the Find window with the keyboard in its query, Enter
    /// searches the project, the options narrow the search, and Enter on the results opens the
    /// selected match with the caret on it.
    #[gpui_kit::test]
    async fn find_in_files_searches_the_project_and_opens_a_match(cx: &mut TestAppContext) {
        use crate::find::session::FindStatus;
        use crate::find::view::{FindOption, FindPanel};
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::create_dir(root.join("src")).expect("a folder");
        std::fs::write(root.join("src/a.txt"), "first line\n  a Needle here\nlast\n").expect("the file is written");
        std::fs::write(root.join("src/b.txt"), "needle one\nnothing\nneedle two, needle three\n")
            .expect("the file is written");
        std::fs::write(root.join("c.txt"), "hay\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));

        cx.update(|window, cx| execute_action_or_throw("toolwindow.find", None, window, cx)).expect("it runs");
        let find = workbench
            .read_with(cx, |w, _| w.tool_window_view(FIND_TOOL_WINDOW))
            .and_then(|view| view.downcast::<FindPanel>().ok())
            .expect("the Find window holds the panel");
        assert!(cx.update(|window, cx| find.focus_handle(cx).is_focused(window)), "the query has the keyboard");
        cx.simulate_input("needle");
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| find.read_with(cx, |find, _| find.status() == FindStatus::Done));
        // Case is ignored by default: four matches on three lines of two files.
        let mut results = find.read_with(cx, |find, _| find.results());
        results.sort();
        assert_eq!(results, [("src/a.txt".to_owned(), vec![1]), ("src/b.txt".to_owned(), vec![0, 2])]);
        assert_eq!(find.read_with(cx, |find, _| find.match_count()), 4);

        // Match Case searches again with it.
        find.update(cx, |find, cx| find.toggle(FindOption::MatchCase, cx));
        settle(cx, |cx| find.read_with(cx, |find, _| find.status() == FindStatus::Done && find.match_count() == 3));
        assert_eq!(find.read_with(cx, |find, _| find.results()), [("src/b.txt".to_owned(), vec![0, 2])]);

        // The same search again lists the same results once, not twice.
        find.update(cx, |find, cx| find.search(cx));
        settle(cx, |cx| find.read_with(cx, |find, _| find.status() == FindStatus::Done));
        assert_eq!(
            find.read_with(cx, |find, _| (find.results(), find.row_count())),
            (vec![("src/b.txt".to_owned(), vec![0, 2])], 3)
        );

        // Down selects the next matching line; Enter opens it with the caret on the match.
        cx.simulate_keystrokes("down enter");
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        settle(cx, |cx| editor.read_with(cx, |editor, _| editor.caret_position() == (2, 0)));
        assert_eq!(editor.read_with(cx, |editor, _| editor.path().to_path_buf()), root.join("src/b.txt"));

        // A pattern that is not a valid expression says so and finds nothing.
        find.update(cx, |find, cx| {
            find.toggle(FindOption::Regex, cx);
            find.field().update(cx, |field, cx| field.set_text("(", cx));
            find.search(cx);
        });
        cx.run_until_parked();
        find.read_with(cx, |find, _| {
            assert_eq!(find.status(), FindStatus::Invalid);
            assert!(find.error().is_some() && find.results().is_empty());
        });
    }

    /// Types `text` into the Search Everywhere field and lets the query rest.
    pub(super) fn type_query(cx: &mut VisualTestContext, text: &str) {
        cx.simulate_input(text);
        cx.executor().advance_clock(crate::everywhere::view::DEBOUNCE);
    }

    /// A project with a Rust file, a text file and a second Rust file, opened in a window.
    pub(super) fn search_project(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, PathBuf, Entity<Workbench>, &mut VisualTestContext) {
        search_project_with(cx, false)
    }

    /// [`search_project`], trusted first when `trusted` (its terminals then start the plain shell).
    fn search_project_with(
        cx: &mut TestAppContext,
        trusted: bool,
    ) -> (tempfile::TempDir, PathBuf, Entity<Workbench>, &mut VisualTestContext) {
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::create_dir(root.join("src")).expect("a folder");
        std::fs::write(
            root.join("src/alpha.rs"),
            "pub struct Widget;\n\npub fn make_widget() -> Widget {\n    Widget\n}\n",
        )
        .expect("the file is written");
        std::fs::write(root.join("src/beta.rs"), "pub fn other() {}\n").expect("the file is written");
        std::fs::write(root.join("notes.txt"), "first\n  a needle here\n").expect("the file is written");
        if trusted {
            cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false))
                .expect("the folder is trusted");
        }
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        // The index lists the files once it has walked the project.
        settle(cx, |cx| {
            let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
            ide_host::ops::nav::find_files(&host, "w1", "alpha".to_owned(), 5).is_ok_and(|found| !found.is_empty())
        });
        (folder, root, workbench, cx)
    }

    fn popup(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Entity<SearchEverywhere> {
        workbench.read_with(cx, |w, _| w.search_everywhere()).expect("Search Everywhere is open")
    }

    /// Go to File: the action opens Search Everywhere on Files with the keyboard in its query,
    /// the typed name finds the file, and Enter opens it and closes the popup. Escape closes it
    /// and gives the keyboard back to the editor.
    #[gpui_kit::test]
    async fn go_to_file_finds_a_file_by_name_and_opens_it(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);

        cx.update(|window, cx| execute_action_or_throw("navigate.gotoFile", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        assert_eq!(everywhere.read_with(cx, |popup, _| popup.tab()), SeTab::Files);
        assert!(cx.update(|window, cx| everywhere.focus_handle(cx).is_focused(window)), "the query has the keyboard");
        cx.run_until_parked();
        assert!(cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("popupOpen")));

        type_query(cx, "alph");
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines() == ["src/alpha.rs"]));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("src/alpha.rs")))
        });
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_none()), "choosing a result closes the popup");

        // Enter typed before the results are in opens the top result of the new query once it is final.
        cx.update(|window, cx| execute_action_or_throw("navigate.gotoFile", None, window, cx)).expect("it runs");
        cx.simulate_input("beta");
        cx.simulate_keystrokes("enter");
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_some()), "Enter waits for the results");
        cx.executor().advance_clock(crate::everywhere::view::DEBOUNCE);
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("src/beta.rs")))
        });

        // `name:line` opens the file with the caret on that line.
        cx.update(|window, cx| execute_action_or_throw("navigate.gotoFile", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        type_query(cx, "alpha:3");
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines() == ["src/alpha.rs"]));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| {
                    e.read(cx).path() == root.join("src/alpha.rs") && e.read(cx).caret_position().0 == 2
                })
            })
        });

        // Escape closes the popup; the keyboard is the editor's again.
        cx.update(|window, cx| execute_action_or_throw("navigate.searchEverywhere", None, window, cx))
            .expect("it runs");
        assert_eq!(popup(&workbench, cx).read_with(cx, |popup, _| popup.tab()), SeTab::All);
        cx.simulate_keystrokes("escape");
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_none()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));
        cx.run_until_parked();
        assert!(!cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("popupOpen")));
    }

    /// Find Action lists the actions matching the query and Enter runs the selected one; Search
    /// Text lists matching lines and opens one with the caret on the match; Tab moves along the
    /// tabs; `/` lists the scope commands and `/files x` moves to Files with `x`.
    #[gpui_kit::test]
    async fn search_everywhere_runs_actions_finds_text_and_switches_tabs(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        cx.update(crate::theme::appearance::init);
        let (_folder, root, workbench, cx) = search_project(cx);

        cx.update(|window, cx| execute_action_or_throw("navigate.findAction", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        assert_eq!(everywhere.read_with(cx, |popup, _| popup.tab()), SeTab::Actions);
        let toggle = action_title(cx, "view.toggleTheme");
        type_query(cx, &toggle);
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines().first() == Some(&toggle)));
        assert!(!cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_none()));

        cx.update(|window, cx| execute_action_or_throw("navigate.searchText", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        type_query(cx, "needle");
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines() == ["notes.txt:2 a needle here"]));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| {
                    e.read(cx).path() == root.join("notes.txt") && e.read(cx).caret_position() == (1, 4)
                })
            })
        });

        // Go to Symbol finds a function by name and opens its file with the caret on the name.
        cx.update(|window, cx| execute_action_or_throw("navigate.gotoSymbol", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        type_query(cx, "make_wid");
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines().len() == 1));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| {
                    e.read(cx).path() == root.join("src/alpha.rs") && e.read(cx).caret_position() == (2, 7)
                })
            })
        });

        // Tab and Shift+Tab move along the tabs; the action for another tab moves the open popup.
        cx.update(|window, cx| execute_action_or_throw("navigate.gotoFile", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        cx.simulate_keystrokes("tab");
        assert_eq!(everywhere.read_with(cx, |popup, _| popup.tab()), SeTab::Symbols);
        cx.simulate_keystrokes("shift-tab shift-tab");
        assert_eq!(everywhere.read_with(cx, |popup, _| popup.tab()), SeTab::Classes);
        cx.update(|window, cx| execute_action_or_throw("navigate.searchText", None, window, cx)).expect("it runs");
        assert_eq!(everywhere.read_with(cx, |popup, _| popup.tab()), SeTab::Text);
        assert_eq!(popup(&workbench, cx), everywhere, "the open popup is the one that moved");

        // `/` lists the scopes; `/files be` moves to Files and searches for `be`.
        cx.simulate_input("/");
        assert_eq!(
            everywhere.read_with(cx, |popup, _| popup.lines()),
            ["/classes", "/files", "/symbols", "/actions", "/text"]
        );
        type_query(cx, "files beta");
        assert_eq!(
            everywhere.read_with(cx, |popup, cx| (popup.tab(), popup.field().read(cx).text().to_owned())),
            (SeTab::Files, "beta".to_owned())
        );
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines() == ["src/beta.rs"]));
    }

    /// A short selection on one line of the editor is what Search Everywhere starts with, selected
    /// so that typing replaces it; a selection over several lines is not taken.
    #[gpui_kit::test]
    async fn search_everywhere_starts_with_the_editor_selection(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        editor.update(cx, |editor, cx| editor.select(editor.offset_of(1, 4), editor.offset_of(1, 10), cx));
        cx.update(|window, cx| execute_action_or_throw("navigate.searchText", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        assert_eq!(everywhere.read_with(cx, |popup, cx| popup.field().read(cx).selected_text().to_owned()), "needle");
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines() == ["notes.txt:2 a needle here"]));
        cx.simulate_keystrokes("escape");

        editor.update(cx, |editor, cx| editor.select(0, editor.offset_of(1, 4), cx));
        cx.update(|window, cx| execute_action_or_throw("navigate.searchText", None, window, cx)).expect("it runs");
        assert_eq!(popup(&workbench, cx).read_with(cx, |popup, cx| popup.field().read(cx).text().to_owned()), "");
    }

    /// The title an action is listed under.
    fn action_title(cx: &mut VisualTestContext, id: &str) -> String {
        cx.update(|_, cx| {
            let action = cx.global::<ActionRegistry>().get(id).expect("the action is registered");
            action.meta.localized(&cx.default_global::<crate::platform::i18n::Localization>().0).title
        })
    }

    /// The selection moves by rows (round the ends, unless Shift is held), by groups of results
    /// and by pages; Show more adds rows and leaves the selection where it was; a click on a row
    /// opens it and a click beside the popup closes it. The files are named so that no action's
    /// title matches the query: only files and lines are listed.
    #[gpui_kit::test]
    async fn search_everywhere_moves_its_selection_shows_more_and_answers_the_mouse(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use gpui_kit::{Modifiers, point, px};
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        for n in 0..10 {
            std::fs::write(root.join(format!("zebra{n}.txt")), "zebra text\n").expect("the file is written");
        }
        settle(cx, |cx| {
            let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
            ide_host::ops::nav::find_files(&host, "w1", "zebra".to_owned(), 50).is_ok_and(|found| found.len() == 10)
        });
        cx.update(|window, cx| execute_action_or_throw("navigate.searchEverywhere", None, window, cx))
            .expect("it runs");
        let everywhere = popup(&workbench, cx);
        type_query(cx, "zebra");
        // Six files, six matching lines, and a "more" row for each.
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| !popup.is_loading() && popup.lines().len() == 14));
        let selected = |cx: &mut VisualTestContext| everywhere.read_with(cx, |popup, _| popup.selected());
        let line = |cx: &mut VisualTestContext, index: usize| {
            everywhere.read_with(cx, |popup, _| popup.lines()[index].clone())
        };
        assert_eq!(selected(cx).as_deref(), Some("zebra0.txt"));

        cx.simulate_keystrokes("down");
        assert_eq!(selected(cx).as_deref(), Some("zebra1.txt"));
        cx.simulate_keystrokes("up up");
        assert_eq!(
            (selected(cx), line(cx, 13)),
            (Some("more".to_owned()), "more".to_owned()),
            "Up at the top goes to the end"
        );
        cx.simulate_keystrokes("down");
        assert_eq!(selected(cx).as_deref(), Some("zebra0.txt"), "Down at the end goes to the top");
        cx.simulate_keystrokes("shift-up");
        assert_eq!(selected(cx).as_deref(), Some("zebra0.txt"), "with Shift the selection stops at the top");

        // The primary modifier with an arrow, and Page Down, go to the next group of results;
        // where there is none, Page Down moves by a page.
        cx.simulate_keystrokes("cmd-down");
        assert_eq!(selected(cx), Some(line(cx, 6)));
        assert!(line(cx, 6).ends_with(":1 zebra text"));
        cx.simulate_keystrokes("cmd-up");
        assert_eq!(selected(cx).as_deref(), Some("zebra0.txt"));
        cx.simulate_keystrokes("pagedown");
        assert_eq!(selected(cx), Some(line(cx, 6)));
        cx.simulate_keystrokes("up up pagedown");
        assert_eq!(selected(cx), Some(line(cx, 6)), "Page Down from the files goes to the lines");

        // Show more of the files: the four that were held back are listed, and the selection is
        // still where the row was, not back at the top.
        cx.simulate_keystrokes("up up up up up up down down down down down down down down down down down down");
        assert_eq!((selected(cx), line(cx, 12)), (Some("more".to_owned()), "more".to_owned()));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| !popup.is_loading() && popup.lines().len() == 17));
        assert_eq!(
            everywhere.read_with(cx, |popup, _| popup.lines()[..10].to_vec()),
            (0..10).map(|n| format!("zebra{n}.txt")).collect::<Vec<_>>()
        );
        assert_eq!(selected(cx), Some(line(cx, 12)));

        // A click on a row opens it.
        let row = cx.debug_bounds("search-everywhere-row-3").expect("the fourth row is drawn");
        cx.simulate_click(row.center(), Modifiers::default());
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("zebra3.txt")))
        });
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_none()));

        // A click beside the popup closes it; one inside it does not.
        cx.update(|window, cx| execute_action_or_throw("navigate.gotoFile", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        type_query(cx, "zebra");
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines().len() == 10));
        let row = cx.debug_bounds("search-everywhere-row-0").expect("the first row is drawn");
        cx.simulate_click(point(row.left() + px(4.0), row.top() - px(40.0)), Modifiers::default());
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_some()), "a click on the tabs' row keeps it");
        cx.simulate_click(point(px(6.0), row.center().y), Modifiers::default());
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_none()));
    }

    /// Closing Search Everywhere gives the keyboard back to what had it, and an action chosen in
    /// it runs in that context: one that needs the terminal's focus runs when the popup was opened
    /// from the terminal. An action that shows a state leaves the popup open. A `/` command row
    /// moves to its tab.
    #[gpui_kit::test]
    async fn search_everywhere_gives_the_keyboard_back_and_runs_actions_where_it_was_opened(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::terminal::panel::TerminalPanel;
        let _home = start(cx);
        let (_folder, _root, workbench, cx) = search_project_with(cx, true);
        cx.update(|window, cx| execute_action_or_throw("toolwindow.terminal", None, window, cx)).expect("it runs");
        let terminals = workbench
            .read_with(cx, |w, _| w.tool_window_view(TERMINAL_TOOL_WINDOW))
            .and_then(|view| view.downcast::<TerminalPanel>().ok())
            .expect("the Terminal window holds its tabs");
        let terminal = terminals.read_with(cx, |panel, _| panel.active_view().cloned()).expect("a first tab");
        cx.run_until_parked();
        assert!(cx.update(|window, cx| terminal.focus_handle(cx).is_focused(window)));

        // Escape: the keyboard is the terminal's again, not the editor's.
        cx.update(|window, cx| execute_action_or_throw("navigate.findAction", None, window, cx)).expect("it runs");
        assert!(!cx.update(|window, cx| terminal.focus_handle(cx).is_focused(window)));
        cx.simulate_keystrokes("escape");
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_none()));
        assert!(cx.update(|window, cx| terminal.focus_handle(cx).is_focused(window)));
        cx.run_until_parked();

        // Close Terminal Session needs the terminal's focus: it is listed as it was when the popup
        // opened, and runs once the popup has given the keyboard back.
        cx.update(|window, cx| execute_action_or_throw("navigate.findAction", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        let close = action_title(cx, "terminal.closeSession");
        type_query(cx, &close);
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines().first() == Some(&close)));
        assert_eq!(terminals.read_with(cx, |panel, _| panel.tab_titles().len()), 1);
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| terminals.read_with(cx, |panel, _| panel.tab_titles().is_empty()));

        // The IDE mode is a state: its action runs and the popup stays.
        let mode = workbench.read_with(cx, |w, cx| w.ide_mode(cx));
        cx.update(|window, cx| execute_action_or_throw("navigate.findAction", None, window, cx)).expect("it runs");
        let everywhere = popup(&workbench, cx);
        let toggle = action_title(cx, "ide.toggleMode");
        type_query(cx, &toggle);
        settle(cx, |cx| everywhere.read_with(cx, |popup, _| popup.lines().first() == Some(&toggle)));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| workbench.read_with(cx, |w, cx| w.ide_mode(cx) != mode));
        assert_eq!(workbench.read_with(cx, |w, _| w.search_everywhere()), Some(everywhere.clone()));

        // `/fi` lists the Files scope; Enter on it moves there with an empty query.
        everywhere.update(cx, |popup, cx| popup.field().update(cx, |field, cx| field.select_all(cx)));
        cx.simulate_input("/fi");
        assert_eq!(everywhere.read_with(cx, |popup, _| popup.lines()), ["/files"]);
        cx.simulate_keystrokes("enter");
        assert_eq!(
            everywhere.read_with(cx, |popup, cx| (popup.tab(), popup.field().read(cx).text().to_owned())),
            (SeTab::Files, String::new())
        );
        cx.simulate_keystrokes("escape");

        // In a window with nothing open the popup opens and closes all the same.
        let (empty, cx) = cx.add_window_view(|window, cx| Workbench::new("w2", window, cx));
        cx.update(|window, cx| execute_action_or_throw("navigate.findAction", None, window, cx)).expect("it runs");
        assert!(empty.read_with(cx, |w, _| w.search_everywhere().is_some()));
        cx.simulate_keystrokes("escape");
        assert!(empty.read_with(cx, |w, _| w.search_everywhere().is_none()));
    }

    /// The menu bar follows the focused window: what cannot run there is greyed out, items show
    /// their action's shortcut, the window's tool windows are listed. A chosen item runs its
    /// action; a key that reaches an item's shortcut after the keymap declined it does not.
    #[gpui_kit::test]
    async fn the_menu_bar_follows_the_window_and_runs_chosen_items(cx: &mut TestAppContext) {
        use menu::{MenuBar, MenuCommand, MenuNode};
        fn item<'a>(nodes: &'a [MenuNode], wanted: &str) -> Option<&'a MenuNode> {
            nodes.iter().find_map(|node| match node {
                MenuNode::Submenu { children, .. } => item(children, wanted),
                MenuNode::Item { action, .. } if action == wanted => Some(node),
                _ => None,
            })
        }
        let shown = |cx: &mut VisualTestContext, wanted: &str| {
            cx.update(|window, cx| {
                menu::sync(window, cx);
                item(cx.global::<MenuBar>().shown().expect("a bar is installed"), wanted).cloned()
            })
        };
        let _home = start(cx);
        cx.update(crate::theme::appearance::init);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.run_until_parked();
        let shortcut = cx.update(|_, cx| {
            let binding = cx.global::<input::Keymap>().0.primary_key_binding("navigate.gotoFile").map(str::to_owned);
            binding.and_then(|binding| menu::menu_accelerator(&binding)).expect("Go to File has a shortcut")
        });
        // Nothing is open: Go to File cannot run.
        assert!(matches!(shown(cx, "navigate.gotoFile"),
            Some(MenuNode::Item { enabled: false, shortcut: Some(shown), .. }) if shown == shortcut));

        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        cx.run_until_parked();
        assert!(matches!(shown(cx, "navigate.gotoFile"), Some(MenuNode::Item { enabled: true, .. })));
        for id in TOOL_WINDOWS {
            let wanted = format!("toolwindow.{id}");
            assert!(matches!(shown(cx, &wanted), Some(MenuNode::Item { enabled: true, .. })), "{wanted} is listed");
        }

        // An item chosen in the bar runs its action in the window.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        assert!(!cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
        cx.dispatch_action(MenuCommand { action: "view.toggleTheme".to_owned(), arg: None });
        cx.run_until_parked();
        assert!(cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
        cx.dispatch_action(MenuCommand { action: "navigate.gotoFile".to_owned(), arg: None });
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_some()));
        cx.simulate_keystrokes("escape");

        // The keymap no longer binds the key the bar still shows for Go to File: pressing it is
        // not the keymap's doing, so the item does not run.
        cx.update(|_, cx| {
            let keymap = &mut cx.global_mut::<input::Keymap>().0;
            keymap.set_overrides(std::collections::BTreeMap::from([("navigate.gotoFile".to_owned(), Vec::new())]));
            assert_eq!(keymap.primary_key_binding("navigate.gotoFile"), None);
        });
        let stroke = menu::gpui_keystroke(&shortcut).expect("the shortcut is one GPUI can press");
        cx.simulate_keystrokes(&stroke);
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_none()));
        // Once the bar has followed the keymap it shows no shortcut for it.
        assert!(matches!(shown(cx, "navigate.gotoFile"), Some(MenuNode::Item { shortcut: None, .. })));
    }

    /// Projects open where "Open project in" says; the window that has a project comes to the
    /// front instead of opening it twice; windows are labelled, cycled and closed over.
    #[gpui_kit::test]
    async fn projects_open_in_this_window_a_new_one_or_the_one_that_has_them(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use windows::Target;
        let _home = start(cx);
        cx.update(gpui_kit::init);
        let folders: Vec<tempfile::TempDir> =
            (0..3).map(|_| tempfile::tempdir().expect("a temporary folder")).collect();
        let roots: Vec<PathBuf> = folders.iter().map(|f| f.path().canonicalize().expect("the folder exists")).collect();
        for root in &roots {
            std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        }
        // Decided folders open without the trust question.
        for root in &roots {
            cx.update(|cx| cx.global::<Backend>().host.trust.decide(root, true, false)).expect("trusted");
        }
        let run = |cx: &mut TestAppContext,
                   window: gpui_kit::AnyWindowHandle,
                   id: &'static str,
                   arg: Option<&std::path::Path>| {
            let arg = arg.map(|path| serde_json::Value::String(path.to_string_lossy().into_owned()));
            window
                .update(cx, |_, window, cx| execute_action_or_throw(id, arg.as_ref(), window, cx))
                .expect("the window is open")
                .expect("the action runs");
            cx.run_until_parked();
        };
        let labels = |cx: &mut TestAppContext| -> Vec<String> {
            cx.update(|cx| windows::windows(cx).into_iter().map(|(label, _)| label).collect())
        };
        let window = |cx: &mut TestAppContext, label: &str| {
            cx.update(|cx| {
                windows::windows(cx).into_iter().find(|(known, _)| known == label).expect("the window is open").1
            })
        };
        let project = |cx: &mut TestAppContext, label: &str| -> Option<PathBuf> {
            let handle = window(cx, label);
            handle
                .update(cx, |_, window, cx| for_window(window, cx).and_then(|w| w.read(cx).project_root(cx)))
                .expect("the window is open")
        };
        let wait = |cx: &mut TestAppContext, done: &dyn Fn(&mut TestAppContext) -> bool| {
            for _ in 0..300 {
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("the backend did not deliver within three seconds");
        };

        // A window that shows nothing takes the project.
        let main = cx.update(|cx| windows::open_window(Target::Empty, cx)).expect("a window opens");
        assert_eq!(labels(cx), ["main"]);
        run(cx, main, "file.openProjectPath", Some(&roots[0]));
        wait(cx, &|cx| project(cx, "main").as_deref() == Some(&roots[0]));

        // The question shows over the window, with "Don't ask again"; its buttons are clicked.
        let asked = |cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle| -> Option<String> {
            window
                .update(cx, |_, window, cx| {
                    let workbench = for_window(window, cx)?;
                    workbench.read(cx).app_dialog().map(|dialog| dialog.read(cx).title().to_owned())
                })
                .expect("the window is open")
        };
        let answer = |cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle, selector: &'static str| {
            let mut visual = VisualTestContext::from_window(window, cx);
            click_on(&mut visual, selector);
        };

        // One with a project asks; New Window opens the next label.
        run(cx, main, "file.openProjectPath", Some(&roots[1]));
        assert_eq!(asked(cx, main).as_deref(), Some("Open Project"));
        answer(cx, main, "prompt-button-new");
        wait(cx, &|cx| labels(cx) == ["main", "project-1"] && project(cx, "project-1").as_deref() == Some(&roots[1]));
        assert_eq!(project(cx, "main").as_deref(), Some(&*roots[0]));

        // A project that another window has is asked about like any other ("Open project in"
        // decides first); This Window then brings that window to the front instead of opening it again.
        let second = window(cx, "project-1");
        run(cx, second, "file.openProjectPath", Some(&roots[0]));
        assert_eq!(asked(cx, second).as_deref(), Some("Open Project"));
        answer(cx, second, "prompt-button-same");
        wait(cx, &|cx| cx.update(|cx| cx.active_window()) == Some(main));
        assert_eq!(labels(cx).len(), 2);
        assert_eq!(project(cx, "project-1").as_deref(), Some(&*roots[1]));

        // Cancel leaves everything as it is; This Window replaces the window's project.
        run(cx, second, "file.openProjectPath", Some(&roots[2]));
        answer(cx, second, "prompt-button-cancel");
        assert!(asked(cx, second).is_none());
        assert_eq!(project(cx, "project-1").as_deref(), Some(&*roots[1]));
        // This Window with "Don't ask again" replaces the project and stores the answer.
        run(cx, second, "file.openProjectPath", Some(&roots[2]));
        answer(cx, second, "prompt-checkbox");
        answer(cx, second, "prompt-button-same");
        wait(cx, &|cx| project(cx, "project-1").as_deref() == Some(&roots[2]));
        assert_eq!(labels(cx).len(), 2);
        assert_eq!(cx.update(|cx| crate::platform::settings::global(cx).general.open_project_in), "same");

        // With the answer stored, nothing is asked.
        run(cx, second, "file.openProjectPath", Some(&roots[1]));
        assert!(asked(cx, second).is_none() && !cx.has_pending_prompt());
        wait(cx, &|cx| project(cx, "project-1").as_deref() == Some(&roots[1]));

        // Open… asks for a folder and opens what was picked the same way.
        run(cx, second, "file.openProject", None);
        assert!(cx.did_prompt_for_paths());
        let picked = roots[2].clone();
        cx.simulate_path_prompt_response(move |options| {
            assert!(options.directories && !options.files);
            Some(vec![picked])
        });
        wait(cx, &|cx| project(cx, "project-1").as_deref() == Some(&roots[2]));

        // Every project that was opened is a recent one, the last first.
        let recent: Vec<PathBuf> = cx.update(|cx| {
            let config = &cx.global::<Backend>().dirs.config;
            ide_workspace::RecentProjects::load(config).list().into_iter().map(|project| project.path.0).collect()
        });
        assert_eq!(recent, [roots[2].clone(), roots[1].clone(), roots[0].clone()]);

        // Next and Previous Project Window go round the windows in label order.
        run(cx, main, "window.new", None);
        assert_eq!(labels(cx), ["main", "project-1", "project-2"]);
        let third = window(cx, "project-2");
        run(cx, third, "window.next", None);
        assert_eq!(cx.update(|cx| cx.active_window()), Some(main));
        run(cx, main, "window.previous", None);
        assert_eq!(cx.update(|cx| cx.active_window()), Some(third));

        // Close Project leaves the window, showing nothing; it takes the next project itself.
        run(cx, second, "file.closeProject", None);
        assert_eq!(project(cx, "project-1"), None);
        assert!(cx.update(|cx| cx.global::<Backend>().host.project("project-1").is_err()));
        run(cx, second, "file.openRecentProject", Some(&roots[1]));
        assert!(!cx.has_pending_prompt());
        wait(cx, &|cx| project(cx, "project-1").as_deref() == Some(&roots[1]));
    }

    /// View › Appearance: each toggle flips its setting and shows as on or off; the status bar and
    /// the tool window bars go when theirs is off, and Compact Mode makes the rows lower. Maximize
    /// gives a tool window's side the whole frame, and the resize actions move its inner edge.
    #[gpui_kit::test]
    async fn appearance_toggles_hide_the_bars_and_tool_windows_maximize_and_resize(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        cx.update(crate::theme::appearance::init);
        let (_folder, root, workbench, cx) = search_project(cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let on = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| cx.global::<ActionRegistry>().get(id).expect("registered").is_toggled(window, cx))
        };
        let appearance =
            |cx: &mut VisualTestContext| cx.update(|_, cx| crate::platform::settings::global(cx).appearance);

        assert_eq!(on(cx, "view.toggleStatusBar"), Some(true));
        assert!(workbench.read_with(cx, |w, cx| w.shows_status_bar(cx)));
        run(cx, "view.toggleStatusBar");
        assert_eq!((on(cx, "view.toggleStatusBar"), appearance(cx).show_status_bar), (Some(false), false));
        assert!(!workbench.read_with(cx, |w, cx| w.shows_status_bar(cx)));
        run(cx, "view.toggleStatusBar");
        assert!(workbench.read_with(cx, |w, cx| w.shows_status_bar(cx)));

        // The stripe's buttons are drawn only while the bars are on.
        assert!(cx.debug_bounds("stripe:project").is_some());
        run(cx, "view.toggleToolWindowBars");
        assert_eq!((on(cx, "view.toggleToolWindowBars"), appearance(cx).show_tool_window_bars), (Some(false), false));
        assert!(cx.debug_bounds("stripe:project").is_none());
        run(cx, "view.toggleToolWindowBars");
        assert!(cx.debug_bounds("stripe:project").is_some());

        let row = |cx: &mut VisualTestContext| cx.update(|_, cx| f32::from(theme::length(cx, "h-row")));
        let tall = row(cx);
        run(cx, "view.toggleCompactMode");
        assert_eq!(on(cx, "view.toggleCompactMode"), Some(true));
        assert!(row(cx) < tall, "compact rows are lower: {} < {tall}", row(cx));
        run(cx, "view.toggleCompactMode");
        assert_eq!(row(cx), tall);
        run(cx, "view.toggleToolbar");
        assert_eq!((on(cx, "view.toggleToolbar"), appearance(cx).show_main_toolbar), (Some(false), false));
        run(cx, "view.toggleToolbar");

        // The Project view has the keyboard: its side grows by a step to the right and shrinks
        // back to the left; maximized it covers the editor, and again restores it.
        run(cx, "toolwindow.project");
        let layout = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| {
                let layout = w.tool_windows().expect("a project is open");
                (layout.sizes().left, layout.maximized())
            })
        };
        assert!(
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("toolWindowFocus"))
        );
        let (width, _) = layout(cx);
        run(cx, "toolwindow.resizeRight");
        assert!(layout(cx).0 > width);
        run(cx, "toolwindow.resizeLeft");
        assert_eq!(layout(cx).0, width);
        run(cx, "toolwindow.maximize");
        assert_eq!(layout(cx).1, Some(Anchor::Left));
        run(cx, "toolwindow.maximize");
        assert_eq!(layout(cx).1, None);
    }

    /// File Structure lists the symbols of the file being edited with the one at the caret
    /// selected; Enter on another puts the caret on it and closes the popup, with the keyboard
    /// back in the editor.
    #[gpui_kit::test]
    async fn file_structure_lists_the_editors_symbols_and_goes_to_the_chosen_one(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        std::fs::write(root.join("Makefile"), "CC = gcc\n\nbuild: main.o\n\t$(CC) -o app main.o\n\nall: build\n")
            .expect("the file is written");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("Makefile"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        editor.update(cx, |editor, cx| {
            let at = editor.offset_of(3, 2);
            editor.select(at, at, cx);
        });

        cx.update(|window, cx| execute_action_or_throw("navigate.fileStructure", None, window, cx)).expect("it runs");
        let structure = workbench.read_with(cx, |w, _| w.file_structure()).expect("File Structure is open");
        settle(cx, |cx| structure.read_with(cx, |popup, _| !popup.is_loading() && !popup.lines().is_empty()));
        assert_eq!(structure.read_with(cx, |popup, _| popup.lines()), ["CC", "build", "all"]);
        assert_eq!(structure.read_with(cx, |popup, _| popup.selected()).as_deref(), Some("build"));

        cx.simulate_keystrokes("down enter");
        settle(cx, |cx| editor.read_with(cx, |editor, _| editor.caret_position() == (5, 0)));
        assert!(workbench.read_with(cx, |w, _| w.file_structure().is_none()));
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));
    }

    /// The Structure tool window lists the symbols of the file being edited, selects the one the
    /// caret moves into, puts the caret on the one Enter is pressed on, and lists a symbol typed into
    /// the file once the edits settle.
    #[gpui_kit::test]
    async fn the_structure_window_follows_the_editor_and_goes_to_the_chosen_symbol(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        std::fs::write(root.join("Makefile"), "CC = gcc\n\nbuild: main.o\n\t$(CC) -o app main.o\n\nall: build\n")
            .expect("the file is written");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("Makefile"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        let caret_at = |cx: &mut VisualTestContext, line: u32, col: u32| {
            editor.update(cx, |editor, cx| {
                let at = editor.offset_of(line, col);
                editor.select(at, at, cx);
            });
            cx.run_until_parked();
        };
        caret_at(cx, 3, 2);

        run_action(cx, "toolwindow.structure", None);
        let view = workbench.read_with(cx, |w, _| w.structure_window_view()).expect("the Structure window is shown");
        let lines = |cx: &mut VisualTestContext| view.read_with(cx, |view, _| view.lines());
        settle(cx, |cx| !lines(cx).is_empty());
        assert_eq!(lines(cx), ["Makefile", "  CC", "  build", "  all"]);

        // With Autoscroll from Source the symbol around the caret is selected, and follows it.
        click_on(cx, "structure-autoscroll-from-source");
        assert_eq!(view.read_with(cx, |view, _| view.selected()).as_deref(), Some("build"));
        caret_at(cx, 5, 1);
        assert_eq!(view.read_with(cx, |view, _| view.selected()).as_deref(), Some("all"));

        // Enter on another symbol puts the caret on it and gives the editor the keyboard.
        click_on(cx, "structure-autoscroll-to-source");
        click_on(cx, "structure-row-1");
        assert_eq!(editor.read_with(cx, |editor, _| editor.caret_position()), (5, 1), "a click only selects");
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| editor.read_with(cx, |editor, _| editor.caret_position() == (0, 0)));
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));

        // A rule typed at the end is listed once the edits settle.
        editor.update(cx, |editor, cx| {
            let end = editor.text().len() as u32;
            editor.insert(Some(end..end), "test: all\n", cx);
        });
        cx.run_until_parked();
        assert_eq!(lines(cx).len(), 4, "the symbols are read again only after the delay");
        settle(cx, |cx| {
            cx.executor().advance_clock(structure_window::RELOAD_DELAY);
            lines(cx).len() == 5
        });
        assert_eq!(lines(cx).last().map(String::as_str), Some("  test"));
    }

    /// The TODO tool window lists the project's TODO comments by file with their counts, Enter opens
    /// the file at the comment, a TODO typed into an open editor shows once its text is scanned, and
    /// Show TODO Items in Current File narrows the list to the editor's file.
    #[gpui_kit::test]
    async fn the_todo_window_lists_the_projects_todo_comments_and_follows_what_is_typed(cx: &mut TestAppContext) {
        use crate::features_a::todo_view::TodoView;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::create_dir(root.join("src")).expect("a folder");
        let (a, b) = (root.join("src/a.rs"), root.join("src/b.rs"));
        std::fs::write(&a, "// TODO first\nfn a() {}\n// FIXME second\n").expect("the file is written");
        std::fs::write(&b, "fn b() {} // TODO third\n").expect("the file is written");
        std::fs::write(root.join("notes.txt"), "no comments here\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));

        run_action(cx, "toolwindow.todo", None);
        let view: Entity<TodoView> =
            workbench.read_with(cx, |w, _| w.todo_window_view()).expect("the TODO tool window is shown");
        let lines = |cx: &mut VisualTestContext| view.update(cx, |view, cx| view.lines(cx));
        settle(cx, |cx| !lines(cx).is_empty());
        assert_eq!(
            lines(cx),
            [
                "a.rs  src  2 items",
                "  (1, 4) TODO first",
                "  (3, 4) FIXME second",
                "b.rs  src  1 item",
                "  (1, 14) TODO third",
            ]
        );

        // Enter on an item opens its file with the caret on the comment.
        click_on(cx, "todo-row-2");
        // The preview pane shows the item's file, read-only, with the caret on the comment.
        settle(cx, |cx| view.read_with(cx, |view, _| view.preview().is_some()));
        let preview = view
            .read_with(cx, |view, _| view.preview().cloned())
            .and_then(|preview| preview.downcast::<Editor>().ok())
            .expect("the preview is an editor");
        preview.read_with(cx, |editor, _| {
            assert!(editor.status().read_only);
            assert_eq!(editor.caret_position(), (2, 3));
            assert_eq!(editor.text(), "// TODO first\nfn a() {}\n// FIXME second\n");
        });
        cx.simulate_keystrokes("enter");
        let at = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, cx| {
                w.editor().map(|e| (e.read(cx).path().to_path_buf(), e.read(cx).caret_position()))
            })
        };
        settle(cx, |cx| at(cx) == Some((a.clone(), (2, 3))));

        // A TODO typed into the editor is listed once the editor's text is scanned.
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        editor.update(cx, |editor, cx| {
            let end = editor.text().len() as u32;
            editor.insert(Some(end..end), "// TODO fourth\n", cx);
        });
        cx.run_until_parked();
        assert_eq!(lines(cx)[0], "a.rs  src  2 items", "the text is scanned only after the delay");
        cx.executor().advance_clock(todo_window::LIVE_SCAN_DELAY);
        cx.run_until_parked();
        assert_eq!(lines(cx)[0], "a.rs  src  3 items");
        assert_eq!(lines(cx)[3], "  (4, 4) TODO fourth");

        // Show TODO Items in Current File: the editor's file alone; Refresh scans the project again.
        run_action(cx, "todo.showCurrentFile", None);
        assert_eq!(view.read_with(cx, |view, _| view.state().tab), crate::features_a::todo::TodoTab::CurrentFile);
        assert_eq!(
            lines(cx),
            ["a.rs  src  3 items", "  (1, 4) TODO first", "  (3, 4) FIXME second", "  (4, 4) TODO fourth"]
        );
        std::fs::write(&b, "fn b() {} // TODO third\n// HACK fifth\n").expect("the file is written");
        run_action(cx, "todo.refresh", None);
        settle(cx, |cx| view.read_with(cx, |view, _| view.badge()) == Some(5));
    }

    /// The build's state as the window's context keys say it: `build.running` and `build.lastStatus`.
    fn build_context(cx: &mut VisualTestContext) -> (bool, Option<String>) {
        use crate::platform::context::{ContextValue, window_context};
        cx.update(|window, cx| {
            let keys = window_context(window, cx);
            let status = match keys.get(crate::build_window::model::BUILD_LAST_STATUS) {
                Some(ContextValue::Text(status)) => Some(status.to_string()),
                _ => None,
            };
            (keys.is_truthy(crate::build_window::model::BUILD_RUNNING), status)
        })
    }

    /// A trusted project of `files` (path, text), open in a window.
    fn trusted_project<'a>(
        cx: &'a mut TestAppContext,
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, PathBuf, Entity<Workbench>, &'a mut VisualTestContext) {
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().expect("a folder")).expect("the folder is made");
            std::fs::write(path, text).expect("the file is written");
        }
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        (folder, root, workbench, cx)
    }

    /// Go to Declaration without a language server (the default Light mode starts none): the
    /// backend's syntax tree resolves a local name to its definition in the same file, and the caret
    /// goes there.
    #[gpui_kit::test]
    async fn go_to_declaration_without_a_server_finds_a_local_through_the_syntax_tree(cx: &mut TestAppContext) {
        let _home = start(cx);
        let text = "function f() {\n  let count = 1;\n  return count;\n}\n";
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/a.js", text)]);
        let file = root.join("src/a.js");
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project_file(file, window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        let usage = text.rfind("count").expect("the usage") as u32 + 2;
        editor.update(cx, |editor, cx| editor.select(usage, usage, cx));
        run_action(cx, "navigate.declaration", None);
        settle(cx, |cx| editor.read_with(cx, |editor, _| editor.caret_position()) == (1, 6));
        let intelligence = cx.update(|_, cx| crate::editor::intelligence::EditorIntelligence::of(&editor, cx));
        assert!(
            intelligence.is_some_and(|intelligence| intelligence.read_with(cx, |this, _| this.chooser().is_none()))
        );
    }

    /// A project without a build system cannot be built: Build Project, Build Module, Rebuild Project
    /// and Build Artifacts each end as a failed build the Build tool window shows with the backend's
    /// reason, notified, with the context keys following the build.
    #[gpui_kit::test]
    async fn building_a_project_without_a_build_system_ends_in_a_failed_build(cx: &mut TestAppContext) {
        use crate::build_window::view::BuildView;
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/alpha.rs", "pub fn alpha() {}\n")]);
        let lines = |cx: &mut VisualTestContext| {
            workbench
                .read_with(cx, |w, _| w.tool_window_view(BUILD_TOOL_WINDOW))
                .and_then(|view| view.downcast::<BuildView>().ok())
                .map(|view| view.read_with(cx, |view, cx| view.lines(cx)))
                .unwrap_or_default()
        };
        assert_eq!(build_context(cx), (false, None));

        // The build starts at once: the context says so before the backend answers.
        cx.update(|window, cx| execute_action_or_throw("build.buildProject", None, window, cx)).expect("it runs");
        assert_eq!(build_context(cx), (true, None));
        settle(cx, |cx| build_context(cx).1.is_some());
        assert_eq!(build_context(cx), (false, Some("failed".to_owned())));
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(BUILD_TOOL_WINDOW)));
        let reason = "no supported build system (Maven, Gradle, Cargo, Go, TypeScript, CMake) found";
        assert_eq!(lines(cx), ["Build  1 error".to_owned(), format!("  {reason}")]);
        let notified = workbench.read_with(cx, |w, _| {
            w.notifications.items().iter().map(|n| (n.title.clone(), n.body.clone())).collect::<Vec<_>>()
        });
        assert_eq!(notified, [("Build failed".to_owned(), Some(reason.to_owned()))]);

        let module = serde_json::json!({ "target": root.join("src/alpha.rs").to_string_lossy() });
        for (id, arg, title) in [
            ("build.buildModule", Some(module), "Build"),
            ("build.rebuildProject", None, "Rebuild"),
            ("build.buildArtifacts", None, "Build Artifacts"),
        ] {
            run_action(cx, id, arg);
            settle(cx, |cx| !build_context(cx).0);
            assert_eq!(build_context(cx).1.as_deref(), Some("failed"), "{id}");
            assert_eq!(lines(cx)[0], format!("{title}  1 error"), "{id}");
        }
    }

    /// A Cargo project builds with the `cargo` the tests run under, offline (it has no dependencies):
    /// a type error fails the build, which lists it under its file and opens it; fixed, the build
    /// succeeds; a build stopped while its build script runs ends cancelled.
    #[gpui_kit::test]
    async fn a_cargo_build_lists_its_error_succeeds_once_fixed_and_ends_cancelled_when_stopped(
        cx: &mut TestAppContext,
    ) {
        use crate::build_window::view::BuildView;
        let _home = start(cx);
        let manifest = "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n";
        let (_folder, root, workbench, cx) =
            trusted_project(cx, &[("Cargo.toml", manifest), ("src/lib.rs", "pub fn f() -> u32 {\n    \"x\"\n}\n")]);
        let view = |cx: &mut VisualTestContext| {
            workbench
                .read_with(cx, |w, _| w.tool_window_view(BUILD_TOOL_WINDOW))
                .and_then(|view| view.downcast::<BuildView>().ok())
                .expect("the Build tool window is shown")
        };
        // A real compiler runs: up to a minute of real time.
        let finished = |cx: &mut VisualTestContext| {
            for _ in 0..6000 {
                cx.run_until_parked();
                if build_context(cx).1.is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("the build did not end within a minute");
        };

        run_action(cx, "build.buildProject", None);
        finished(cx);
        let lines = view(cx).read_with(cx, |view, cx| view.lines(cx));
        assert_eq!(build_context(cx).1.as_deref(), Some("failed"), "{lines:?}");
        assert!(lines[0].ends_with("  1 error"), "{lines:?}");
        assert_eq!(lines[1], "  lib.rs  src  1 error", "{lines:?}");
        assert!(lines[2].starts_with("    mismatched types") && lines[2].ends_with(":2:5"), "{lines:?}");
        // The first error is selected and its place opened.
        let lib = root.join("src/lib.rs");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| e.read(cx).path() == lib && e.read(cx).caret_position() == (1, 4))
            })
        });
        let detail = view(cx).read_with(cx, |view, cx| view.detail_text(cx));
        assert!(detail.is_some_and(|text| text.starts_with("mismatched types")));

        std::fs::write(&lib, "pub fn f() -> u32 {\n    1\n}\n").expect("the file is written");
        // A new build clears the last outcome while it runs.
        run_action(cx, "build.buildProject", None);
        finished(cx);
        let lines = view(cx).read_with(cx, |view, cx| view.lines(cx));
        assert_eq!(build_context(cx), (false, Some("success".to_owned())), "{lines:?}");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("  finished "), "{lines:?}");

        // A build script that sleeps keeps the build running until it is stopped.
        std::fs::write(
            root.join("build.rs"),
            "fn main() { std::thread::sleep(std::time::Duration::from_secs(120)); }\n",
        )
        .expect("the file is written");
        run_action(cx, "build.buildProject", None);
        assert!(build_context(cx).0, "the build runs");
        // The build script runs once cargo reports compiling the package.
        for _ in 0..6000 {
            cx.run_until_parked();
            let console = view(cx).read_with(cx, |view, cx| view.console_lines(cx));
            if console.iter().any(|line| line.contains("Compiling probe")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        run_action(cx, "build.stop", None);
        finished(cx);
        assert_eq!(build_context(cx), (false, Some("cancelled".to_owned())));
        assert!(view(cx).read_with(cx, |view, cx| view.lines(cx))[0].ends_with("  cancelled"));
    }

    /// Recent Files lists the project's files by when they were last shown and starts on the
    /// previous one, so the action and Enter go back and forth between two files; Delete closes
    /// the selected file's tab and leaves the popup open. The Switcher opens with Ctrl held, moves
    /// on every press and opens its selection when Ctrl is released.
    #[gpui_kit::test]
    async fn recent_files_and_the_switcher_go_between_the_editors(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use gpui_kit::Modifiers;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let showing = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, cx| w.editor().map(|editor| editor.read(cx).path().to_path_buf()))
        };
        for name in ["src/alpha.rs", "src/beta.rs", "notes.txt"] {
            workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join(name), window, cx));
            settle(cx, |cx| showing(cx) == Some(root.join(name)));
        }
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let recent =
            |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.recent_files()).expect("the popup is open");

        run(cx, "navigate.recentFiles");
        let popup = recent(cx);
        settle(cx, |cx| popup.read_with(cx, |popup, _| !popup.is_loading()));
        assert_eq!(popup.read_with(cx, |popup, _| popup.lines()), ["notes.txt", "src/beta.rs", "src/alpha.rs"]);
        let beta = root.join("src/beta.rs").to_string_lossy().into_owned();
        assert_eq!(popup.read_with(cx, |popup, _| popup.selected_file()), Some(beta));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| showing(cx) == Some(root.join("src/beta.rs")));
        assert!(workbench.read_with(cx, |w, _| w.recent_files().is_none()));

        // The file that was showing is now the previous one: the same keys go back to it.
        run(cx, "navigate.recentFiles");
        let popup = recent(cx);
        settle(cx, |cx| popup.read_with(cx, |popup, _| !popup.is_loading()));
        assert_eq!(popup.read_with(cx, |popup, _| popup.lines()), ["src/beta.rs", "notes.txt", "src/alpha.rs"]);
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| showing(cx) == Some(root.join("notes.txt")));

        // Delete closes the tab of the selected file; the popup stays, with the keyboard.
        run(cx, "navigate.recentFiles");
        let popup = recent(cx);
        settle(cx, |cx| popup.read_with(cx, |popup, _| !popup.is_loading()));
        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        assert_eq!(popup.read_with(cx, |popup, _| popup.lines()), ["notes.txt", "src/alpha.rs"]);
        assert_eq!(workbench.read_with(cx, |w, _| w.tab_titles().len()), 2);
        assert!(cx.update(|window, cx| popup.focus_handle(cx).is_focused(window)));
        cx.simulate_keystrokes("escape");
        assert!(workbench.read_with(cx, |w, _| w.recent_files().is_none()));

        // The Switcher: Ctrl is down as it opens, on the editor used before this one; a second
        // press moves on, and letting go of Ctrl opens the selection.
        cx.simulate_modifiers_change(Modifiers { control: true, ..Modifiers::default() });
        run(cx, "navigate.switcher");
        let popup = recent(cx);
        settle(cx, |cx| popup.read_with(cx, |popup, _| !popup.is_loading()));
        assert!(popup.read_with(cx, |popup, _| popup.is_switcher()));
        assert_eq!(popup.read_with(cx, |popup, _| popup.lines()), ["notes.txt", "src/alpha.rs"]);
        let alpha = root.join("src/alpha.rs").to_string_lossy().into_owned();
        assert_eq!(popup.read_with(cx, |popup, _| popup.selected_file()), Some(alpha));
        run(cx, "navigate.switcher");
        assert_eq!(popup.read_with(cx, |popup, _| popup.selected_file()), None, "past the files come the tool windows");
        run(cx, "navigate.switcherBack");
        cx.simulate_modifiers_change(Modifiers::default());
        settle(cx, |cx| showing(cx) == Some(root.join("src/alpha.rs")));
        assert!(workbench.read_with(cx, |w, _| w.recent_files().is_none()));
    }

    /// The bar follows the focused window by itself: a change of what the window can do reaches
    /// it once the window has rested, and a draw that changes nothing asks for nothing.
    #[gpui_kit::test]
    async fn the_menu_bar_follows_the_focused_window_when_it_changes(cx: &mut TestAppContext) {
        use menu::{MenuBar, MenuNode};
        fn enabled(nodes: &[MenuNode], wanted: &str) -> Option<bool> {
            nodes.iter().find_map(|node| match node {
                MenuNode::Submenu { children, .. } => enabled(children, wanted),
                MenuNode::Item { action, enabled, .. } if action == wanted => Some(*enabled),
                _ => None,
            })
        }
        let go_to_file = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| cx.global::<MenuBar>().shown().and_then(|bar| enabled(bar, "navigate.gotoFile")))
        };
        let rest = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.executor().advance_clock(menu::SYNC_DELAY);
            cx.run_until_parked();
        };
        let _home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        rest(cx);
        assert_eq!(go_to_file(cx), Some(false), "nothing is open");

        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        rest(cx);
        assert_eq!(go_to_file(cx), Some(true), "the project made it possible");

        // What changed while the bar was waiting is followed by one more look; after that, drawing
        // again without a change leaves the bar alone: no new request is pending.
        rest(cx);
        assert!(!cx.update(|_, cx| cx.global::<MenuBar>().is_sync_pending()));
        workbench.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(!cx.update(|_, cx| cx.global::<MenuBar>().is_sync_pending()));
    }

    /// Split Right shows the file a second time beside itself, at the same place and with the
    /// keyboard; what is typed in one pane shows in the other. Tabs move and open across the
    /// panes, the keyboard goes from pane to pane, a click in a pane makes it the active one, and
    /// Unsplit takes a pane away again.
    #[gpui_kit::test]
    async fn the_editor_splits_into_panes_that_show_and_edit_the_same_file(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use gpui_kit::Modifiers;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("src/alpha.rs"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let panes = |cx: &mut VisualTestContext| -> Vec<(bool, Vec<String>)> {
            workbench.read_with(cx, |w, _| {
                let tabs = w.tabs().expect("a project is open");
                let ids: Vec<String> = tabs.pane_ids().into_iter().map(str::to_owned).collect();
                ids.iter()
                    .map(|id| (*id == tabs.active_pane, w.tab_titles_of(id).into_iter().map(|tab| tab.name).collect()))
                    .collect()
            })
        };
        let split = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("editorSplit"))
        };
        let first = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        first.update(cx, |editor, cx| {
            let at = editor.offset_of(2, 4);
            editor.select(at, at, cx);
        });
        assert!(!split(cx));

        run(cx, "editor.splitRight");
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some_and(|editor| *editor != first)));
        assert_eq!(panes(cx), [(false, vec!["alpha.rs".to_owned()]), (true, vec!["alpha.rs".to_owned()])]);
        let second = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("the new pane's editor");
        assert_eq!(second.read_with(cx, |editor, _| editor.caret_position()), (2, 4), "it opens where the first was");
        assert!(cx.update(|window, cx| second.focus_handle(cx).is_focused(window)));
        cx.run_until_parked();
        assert!(split(cx));

        // Both panes are views of one document.
        cx.simulate_input("xyz");
        settle(cx, |cx| first.read_with(cx, |editor, _| editor.text().contains("xyz")));
        assert_eq!(first.read_with(cx, |editor, _| editor.text()), second.read_with(cx, |editor, _| editor.text()));

        // Go to Next Splitter hands the keyboard to the other pane, and round again.
        run(cx, "editor.nextSplitter");
        assert!(cx.update(|window, cx| first.focus_handle(cx).is_focused(window)));
        assert_eq!(panes(cx).iter().map(|pane| pane.0).collect::<Vec<_>>(), [true, false]);
        run(cx, "editor.prevSplitter");
        assert!(cx.update(|window, cx| second.focus_handle(cx).is_focused(window)));

        // A file opened in the active pane moves to the other with Move to Opposite Group.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("src/beta.rs"), window, cx));
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path().ends_with("beta.rs")))
        });
        assert_eq!(panes(cx)[1], (true, vec!["alpha.rs".to_owned(), "beta.rs".to_owned()]));
        run(cx, "editor.moveToOppositeGroup");
        assert_eq!(
            panes(cx),
            [(true, vec!["alpha.rs".to_owned(), "beta.rs".to_owned()]), (false, vec!["alpha.rs".to_owned()])]
        );

        // A click on the other pane's tab makes that pane the active one.
        let other_tab = workbench.read_with(cx, |w, _| {
            let tabs = w.tabs().expect("a project is open");
            let pane = tabs.pane(tabs.pane_ids()[1]).expect("the second pane");
            pane.tabs[0].id.clone()
        });
        let tab = cx.debug_bounds(Box::leak(format!("tab:{other_tab}").into_boxed_str())).expect("the tab is drawn");
        cx.simulate_click(tab.center(), Modifiers::default());
        assert_eq!(panes(cx).iter().map(|pane| pane.0).collect::<Vec<_>>(), [false, true]);

        // The splitter turns, and Unsplit closes the active pane.
        let stacked = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| matches!(&w.tabs().expect("tabs").layout, LayoutNode::Split { dir, .. } if *dir == SplitDir::Column))
        };
        assert!(!stacked(cx));
        run(cx, "editor.changeSplitOrientation");
        assert!(stacked(cx));
        run(cx, "editor.unsplit");
        assert_eq!(panes(cx), [(true, vec!["alpha.rs".to_owned(), "beta.rs".to_owned()])]);
        cx.run_until_parked();
        assert!(!split(cx));

        // Open in Opposite Group splits when there is no other pane; Unsplit All keeps this one.
        run(cx, "editor.openInOppositeGroup");
        settle(cx, |cx| panes(cx).len() == 2);
        run(cx, "editor.splitDown");
        settle(cx, |cx| panes(cx).len() == 3);
        run(cx, "editor.unsplitAll");
        assert_eq!(panes(cx).len(), 1);
    }

    /// A right click on a tab opens its menu at the pointer. Its rows close tabs around it, pin
    /// it and split the editor; what cannot be done is greyed out; Escape gives the keyboard back.
    #[gpui_kit::test]
    async fn a_tabs_menu_closes_pins_and_splits(cx: &mut TestAppContext) {
        use gpui_kit::{Modifiers, MouseButton};
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        for name in ["src/alpha.rs", "src/beta.rs", "notes.txt"] {
            workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join(name), window, cx));
            settle(cx, |cx| {
                workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join(name)))
            });
        }
        let names = |cx: &mut VisualTestContext| -> Vec<String> {
            workbench.read_with(cx, |w, _| w.tab_titles().into_iter().map(|tab| tab.name).collect())
        };
        let tab_id = |cx: &mut VisualTestContext, index: usize| {
            workbench.read_with(cx, |w, _| {
                let tabs = w.tabs().expect("a project is open");
                tabs.pane(&tabs.active_pane).expect("the active pane").tabs[index].id.clone()
            })
        };
        let right_click = |cx: &mut VisualTestContext, index: usize| {
            let id = tab_id(cx, index);
            let tab = cx.debug_bounds(Box::leak(format!("tab:{id}").into_boxed_str())).expect("the tab is drawn");
            cx.simulate_mouse_down(tab.center(), MouseButton::Right, Modifiers::default());
            cx.simulate_mouse_up(tab.center(), MouseButton::Right, Modifiers::default());
            workbench.read_with(cx, |w, _| w.open_menu::<TabCommand>()).expect("the tab's menu is open")
        };
        let labels = |menu: &Entity<PopupMenu<TabCommand>>, cx: &mut VisualTestContext| -> Vec<(String, bool)> {
            menu.read_with(cx, |menu, _| menu.rows().iter().map(|row| (row.label.clone(), row.disabled)).collect())
        };

        // The first tab has nothing to its left; one pane has no opposite group.
        let menu = right_click(cx, 0);
        let rows = labels(&menu, cx);
        assert_eq!(
            rows.iter().map(|(label, _)| label.as_str()).collect::<Vec<_>>(),
            [
                "Close",
                "Close Other Tabs",
                "Close Tabs to the Left",
                "Close Tabs to the Right",
                "Close All Tabs",
                "Close Unmodified Tabs",
                "Pin Tab",
                "Split and Move Right",
                "Split and Move Down",
                "Split Right",
                "Split Down",
                // The file's rows whose actions are registered: the bookmark group, Open In, Git, Copy
                // Path/Reference… (Copy Absolute Path, File Name, Path From Content Root) and Rename File…
                "Add Bookmark",
                "Open In",
                "Git",
                "Copy Path/Reference…",
                "Rename File…",
                // File › Settings… on the Editor Tabs page.
                "Configure Editor Tabs…",
            ]
        );
        assert_eq!(
            rows.iter().filter(|(_, disabled)| *disabled).map(|(label, _)| label.as_str()).collect::<Vec<_>>(),
            ["Close Tabs to the Left"]
        );
        assert!(cx.update(|window, cx| menu.focus_handle(cx).is_focused(window)));
        // Escape closes the menu and the editor has the keyboard again.
        cx.simulate_keystrokes("escape");
        assert!(workbench.read_with(cx, |w, _| w.open_menu::<TabCommand>().is_none()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));

        // Close Tabs to the Right, chosen with the keyboard, on the middle tab.
        right_click(cx, 1);
        cx.simulate_keystrokes("down down down enter");
        cx.run_until_parked();
        assert_eq!(names(cx), ["alpha.rs", "beta.rs"]);

        // Pin Tab pins it, and the menu then offers to unpin and to close all but the pinned.
        right_click(cx, 1);
        cx.simulate_keystrokes("p enter");
        cx.run_until_parked();
        let pinned = workbench.read_with(cx, |w, _| {
            let tabs = w.tabs().expect("a project is open");
            tabs.pane(&tabs.active_pane).expect("the active pane").tabs.iter().filter(|tab| tab.pinned).count()
        });
        assert_eq!(pinned, 1);
        let menu = right_click(cx, 0);
        let rows: Vec<String> = labels(&menu, cx).into_iter().map(|(label, _)| label).collect();
        assert!(rows.contains(&"Unpin Tab".to_owned()) && rows.contains(&"Close All but Pinned".to_owned()));
        cx.simulate_keystrokes("escape");

        // Split Right from the menu of a tab that is not the selected one: that tab is split.
        let menu = right_click(cx, 0);
        let first = names(cx)[0].clone();
        menu.update(cx, |_, cx| cx.emit(PopupMenuEvent::Chosen(Choice::Command(TabCommand::SplitRight))));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.tabs().is_some_and(|tabs| tabs.panes.len() == 2)));
        let split: Vec<Vec<String>> = workbench.read_with(cx, |w, _| {
            let tabs = w.tabs().expect("a project is open");
            tabs.pane_ids()
                .into_iter()
                .map(|id| w.tab_titles_of(id).into_iter().map(|tab| tab.name).collect())
                .collect()
        });
        assert_eq!(split[1], [first]);
        // With two panes the menu has the rows for the other group.
        let menu = right_click(cx, 0);
        assert!(labels(&menu, cx).iter().any(|(label, _)| label == "Move to Opposite Group"));
    }

    /// Go to Line puts the caret on the typed line and column. Bookmarks are toggled at the caret
    /// or at a given line, stored with the project, visited in order and by their digit, and
    /// listed in the Bookmarks popup, whose Enter opens one and whose Delete removes it.
    #[gpui_kit::test]
    async fn go_to_line_and_bookmarks_move_the_caret_through_the_project(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let run = |cx: &mut VisualTestContext, id: &str, arg: Option<serde_json::Value>| {
            cx.update(|window, cx| execute_action_or_throw(id, arg.as_ref(), window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let at = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, cx| {
                w.editor().map(|editor| (editor.read(cx).path().to_path_buf(), editor.read(cx).caret_position()))
            })
        };
        let marks = |cx: &mut VisualTestContext| -> Vec<(String, u32, Option<char>)> {
            workbench.read_with(cx, |w, _| {
                let strip = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
                w.bookmarks().data().items.iter().map(|b| (strip(&b.path), b.line, b.mnemonic)).collect()
            })
        };
        let (alpha, beta) = (root.join("src/alpha.rs"), root.join("src/beta.rs"));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(alpha.clone(), window, cx));
        settle(cx, |cx| at(cx).is_some());

        run(cx, "navigate.gotoLine", None);
        assert!(workbench.read_with(cx, |w, _| w.popup.is_some()));
        cx.simulate_input("3:5");
        cx.simulate_keystrokes("enter");
        assert_eq!(at(cx), Some((alpha.clone(), (2, 4))));
        assert!(workbench.read_with(cx, |w, _| w.popup.is_none()));

        // A bookmark at the caret, one on a line of a file that is not open, one with a digit.
        run(cx, "navigate.toggleBookmark", None);
        let other = serde_json::json!({ "path": beta.to_string_lossy(), "line": 0 });
        run(cx, "navigate.toggleBookmark", Some(other));
        assert_eq!(marks(cx), [("alpha.rs".to_owned(), 2, None), ("beta.rs".to_owned(), 0, None)]);
        let stored = cx.update(|_, cx| {
            let key = bookmarks_key(&root.to_string_lossy());
            cx.global::<UiStorage>().get(&key).map(str::to_owned)
        });
        assert!(stored.is_some_and(|stored| stored.contains("beta.rs")), "the bookmarks are stored with the project");

        // Next Bookmark goes on in file and line order and round again.
        run(cx, "navigate.nextBookmark", None);
        settle(cx, |cx| at(cx) == Some((beta.clone(), (0, 0))));
        run(cx, "navigate.toggleBookmark7", None);
        assert_eq!(marks(cx)[1], ("beta.rs".to_owned(), 0, Some('7')));
        run(cx, "navigate.nextBookmark", None);
        settle(cx, |cx| at(cx) == Some((alpha.clone(), (2, 0))));
        run(cx, "navigate.gotoBookmark7", None);
        settle(cx, |cx| at(cx) == Some((beta.clone(), (0, 0))));

        // The popup lists them; Delete removes the selected one, Enter opens the one left.
        run(cx, "navigate.showBookmarks", None);
        let popup = workbench
            .read_with(cx, |w, _| {
                w.popup.as_ref().and_then(|popup| popup.view.clone().downcast::<BookmarksPopup>().ok())
            })
            .expect("the Bookmarks popup is open");
        let listed = popup.read_with(cx, |popup, _| popup.lines().join("\n"));
        assert!(listed.contains("alpha.rs") && listed.contains("beta.rs"), "both files are listed:\n{listed}");
        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        assert_eq!(marks(cx).len(), 1, "what the popup removed is gone from the window's bookmarks");
        let left = marks(cx)[0].clone();
        cx.simulate_keystrokes("down up enter");
        settle(cx, |cx| at(cx).is_some_and(|(path, (line, _))| path.ends_with(&left.0) && line == left.1));
        assert!(workbench.read_with(cx, |w, _| w.popup.is_none()));

        // Toggling where a bookmark is removes it.
        run(cx, "navigate.toggleBookmark", None);
        assert!(marks(cx).is_empty());
    }

    /// A bookmark toggled in the editor shows in the Bookmarks tool window, whose Enter opens the
    /// file at its line; a list renamed and a bookmark deleted there are stored with the project, so a
    /// new window on it lists them so.
    #[gpui_kit::test]
    async fn the_bookmarks_window_lists_opens_renames_and_deletes_what_the_project_stores(cx: &mut TestAppContext) {
        use crate::everywhere::bookmarks_window::BookmarksWindow;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
        let (alpha, beta) = (root.join("src/alpha.rs"), root.join("src/beta.rs"));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(beta.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        run_action(cx, "toolwindow.bookmarks", None);
        let view =
            workbench.read_with(cx, |w, _| w.bookmarks_window_view()).expect("the Bookmarks tool window is shown");
        let lines = |cx: &mut VisualTestContext| view.read_with(cx, |view, cx| view.lines(cx));
        let name = root.file_name().expect("a name").to_string_lossy().into_owned();

        // Toggled in the editor, at the caret and at a line of another file.
        run_action(cx, "navigate.toggleBookmark", None);
        run_action(
            cx,
            "navigate.toggleBookmark",
            Some(serde_json::json!({ "path": alpha.to_string_lossy(), "line": 2 })),
        );
        let shown = lines(cx);
        assert_eq!(shown[0], format!("{name}  2 bookmarks · default"), "{shown:?}");
        assert!(shown.iter().any(|line| line.trim_start().starts_with("alpha.rs:3")), "{shown:?}");
        assert!(shown.iter().any(|line| line.trim_start().starts_with("beta.rs:1")), "{shown:?}");

        // Enter on the bookmark opens its file with the caret at its line.
        click_on(cx, leak(format!("bookmarks-row:b:{}:2", alpha.to_string_lossy())));
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| e.read(cx).path() == alpha && e.read(cx).caret_position().0 == 2)
            })
        });

        // The list renamed in place, the bookmark of beta.rs deleted.
        click_on(cx, "bookmarks-row:l:default");
        cx.simulate_keystrokes("f2");
        cx.simulate_input("Mine");
        cx.simulate_keystrokes("enter");
        click_on(cx, leak(format!("bookmarks-row:b:{}:0", beta.to_string_lossy())));
        cx.simulate_keystrokes("delete");
        cx.run_until_parked();
        let names = |w: &Workbench| -> (Vec<String>, Vec<u32>) {
            let data = w.bookmarks().data();
            (data.lists.iter().map(|list| list.name.clone()).collect(), data.items.iter().map(|b| b.line).collect())
        };
        assert_eq!(workbench.read_with(cx, |w, _| names(w)), (vec!["Mine".to_owned()], vec![2]));
        // Both changes are stored with the project at once.
        let stored = |cx: &mut VisualTestContext, key: String| {
            cx.update(|_, cx| cx.global::<UiStorage>().get(&key).map(str::to_owned)).unwrap_or_default()
        };
        let scope = root.to_string_lossy().into_owned();
        let (items, lists) = (stored(cx, bookmarks_key(&scope)), stored(cx, lists_key(&scope)));
        assert!(items.contains("alpha.rs") && !items.contains("beta.rs"), "{items}");
        assert!(lists.contains("\"Mine\""), "{lists}");

        // Toggled off and on again at the caret in the editor, the window follows.
        run_action(cx, "navigate.toggleBookmark", None);
        assert_eq!(lines(cx), ["Mine  0 bookmarks · default"]);
        run_action(cx, "navigate.toggleBookmark", None);
        assert_eq!(lines(cx)[0], "Mine  1 bookmark · default");

        // New Bookmark List names the list in the window.
        run_action(cx, "bookmarks.newList", None);
        cx.simulate_input("Later");
        cx.simulate_keystrokes("enter");
        assert_eq!(lines(cx).last().map(String::as_str), Some("Later  0 bookmarks"));

        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
        let (second, cx2) = cx.add_window_view(|window, cx| Workbench::new("w2", window, cx));
        second.update_in(cx2, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx2, |cx| second.read_with(cx, |w, _| w.project_tree().is_some()));
        run_action(cx2, "toolwindow.bookmarks", None);
        let reopened = second
            .read_with(cx2, |w, _| w.tool_window_view(BOOKMARKS_TOOL_WINDOW))
            .and_then(|view| view.downcast::<BookmarksWindow>().ok())
            .expect("the Bookmarks tool window is shown");
        let listed = reopened.read_with(cx2, |view, cx| view.lines(cx));
        assert_eq!(listed[0], "Mine  1 bookmark · default", "{listed:?}");
        assert_eq!(listed.len(), 4, "the list, alpha.rs, its line and the new list: {listed:?}");
        assert!(listed[2].trim_start().starts_with("alpha.rs:3"), "{listed:?}");
        assert_eq!(listed[3], "Later  0 bookmarks");
    }

    /// Find in Files opens as a popup that searches as the query is typed; Enter opens the
    /// selected match and closes it. Open in Find Window hands the search to the Find tool
    /// window, and Replace in Files opens the same popup in its replace mode.
    #[gpui_kit::test]
    async fn find_in_files_opens_a_popup_that_searches_opens_and_hands_over(cx: &mut TestAppContext) {
        use crate::find::popup_view::SEARCH_DEBOUNCE;
        use crate::find::session::FindStatus;
        use crate::find::view::FindPanel;
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let found = |popup: &Entity<FindPopup>, cx: &mut VisualTestContext| {
            for _ in 0..300 {
                cx.executor().advance_clock(SEARCH_DEBOUNCE);
                cx.run_until_parked();
                if popup.read_with(cx, |popup, _| popup.status() == FindStatus::Done) {
                    return popup.read_with(cx, |popup, _| popup.lines());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("the search did not finish within three seconds");
        };

        run(cx, "find.inPath");
        let popup = workbench.read_with(cx, |w, _| w.find_popup()).expect("Find in Files is open");
        assert!(!popup.read_with(cx, |popup, _| popup.is_replace()));
        assert!(cx.update(|window, cx| popup.focus_handle(cx).is_focused(window)), "the query has the keyboard");
        assert!(
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("findPopupOpen"))
        );
        cx.simulate_input("needle");
        let lines = found(&popup, cx);
        assert_eq!(lines.len(), 1, "one line matches: {lines:?}");
        assert!(lines[0].contains("notes.txt") && lines[0].contains("a needle here"), "{lines:?}");
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| {
                    e.read(cx).path() == root.join("notes.txt") && e.read(cx).caret_position() == (1, 4)
                })
            })
        });
        assert!(workbench.read_with(cx, |w, _| w.find_popup().is_none()));
        cx.run_until_parked();
        assert!(
            !cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("findPopupOpen"))
        );

        // Open in Find Window: the popup goes and the tool window runs the same search.
        run(cx, "find.inPath");
        let popup = workbench.read_with(cx, |w, _| w.find_popup()).expect("Find in Files is open");
        popup.update(cx, |popup, cx| popup.set_pattern("widget", cx));
        found(&popup, cx);
        popup.update(cx, |popup, cx| popup.open_in_find_window(cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.find_popup().is_none() && w.is_tool_window_visible(FIND_TOOL_WINDOW)));
        let panel = workbench
            .read_with(cx, |w, _| w.tool_window_view(FIND_TOOL_WINDOW))
            .and_then(|view| view.downcast::<FindPanel>().ok())
            .expect("the Find window holds the panel");
        settle(cx, |cx| panel.read_with(cx, |panel, _| panel.status() == FindStatus::Done));
        assert_eq!(panel.read_with(cx, |panel, cx| panel.field().read(cx).text().to_owned()), "widget");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.results().into_iter().map(|(file, _)| file).collect::<Vec<_>>()),
            ["src/alpha.rs"]
        );

        // Replace in Files is the same popup with a replacement; Find in Files asked for while
        // it is open turns that popup back.
        run(cx, "find.replaceInPath");
        let popup = workbench.read_with(cx, |w, _| w.find_popup()).expect("Replace in Files is open");
        assert!(popup.read_with(cx, |popup, _| popup.is_replace()));
        run(cx, "find.inPath");
        assert_eq!(workbench.read_with(cx, |w, _| w.find_popup()), Some(popup.clone()));
        assert!(!popup.read_with(cx, |popup, _| popup.is_replace()));
    }

    /// Find opens a bar above the file's editor with the keyboard in it; typing selects the first
    /// occurrence, Enter and Find Next go on to the next ones, Replace shows the replacement and replaces
    /// the selected occurrence, and Escape closes the bar with the keyboard back in the editor. The bar
    /// belongs to its tab: another tab has none.
    #[gpui_kit::test]
    async fn find_opens_a_bar_over_the_tabs_editor_that_searches_replaces_and_closes(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::platform::context::window_context;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let key =
            |cx: &mut VisualTestContext, key: &str| cx.update(|window, cx| window_context(window, cx).is_truthy(key));
        let open = |cx: &mut VisualTestContext, name: &str| {
            let path = root.join(name);
            workbench.update_in(cx, |w, window, cx| w.open_project_file(path.clone(), window, cx));
            settle(cx, |cx| {
                workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == path))
                    && cx.update(|window, cx| {
                        workbench.read(cx).editor().is_some_and(|e| e.focus_handle(cx).is_focused(window))
                    })
            });
            workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor")
        };
        let editor = open(cx, "src/alpha.rs");
        let selection = |cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.selection());

        run(cx, "find.inFile");
        let bar = workbench.read_with(cx, |w, _| w.find_bar()).expect("the bar is open");
        assert!(!bar.read_with(cx, |bar, _| bar.is_replace()));
        assert!(cx.update(|window, cx| bar.read(cx).find_field().focus_handle(cx).is_focused(window)));
        assert!(key(cx, "findBarOpen") && key(cx, "findBarFocus"));

        // "pub struct Widget;\n\npub fn make_widget() -> Widget {\n    Widget\n}\n"
        cx.simulate_input("widget");
        cx.run_until_parked();
        assert_eq!(selection(cx), 11..17);
        assert_eq!(bar.read_with(cx, |bar, _| bar.match_count()), 4);
        assert!(key(cx, "findBarHasResults"));
        cx.simulate_keystrokes("enter");
        assert_eq!(selection(cx), 32..38);
        run(cx, "find.next");
        assert_eq!(selection(cx), 44..50);
        run(cx, "find.previous");
        assert_eq!(selection(cx), 32..38);
        run(cx, "find.next");

        // Replace turns the same bar into the replace bar; Replace changes the selected occurrence.
        run(cx, "find.replaceInFile");
        assert_eq!(workbench.read_with(cx, |w, _| w.find_bar()), Some(bar.clone()));
        assert!(bar.read_with(cx, |bar, _| bar.is_replace()));
        bar.update(cx, |bar, cx| bar.set_replacement("gadget", cx));
        bar.update(cx, FindBar::replace_one);
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |e, _| e.text()),
            "pub struct Widget;\n\npub fn make_widget() -> gadget {\n    Widget\n}\n"
        );

        // Another tab has no bar; coming back finds this one as it was.
        open(cx, "notes.txt");
        assert_eq!(workbench.read_with(cx, |w, _| w.find_bar()), None);
        assert!(!key(cx, "findBarOpen"));
        open(cx, "src/alpha.rs");
        assert_eq!(workbench.read_with(cx, |w, _| w.find_bar()), Some(bar.clone()));

        // Escape in the bar closes it and gives the editor the keyboard.
        run(cx, "find.inFile");
        assert!(cx.update(|window, cx| bar.read(cx).has_focus(window, cx)));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(workbench.read_with(cx, |w, _| w.find_bar()), None);
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));
        assert!(!key(cx, "findBarOpen") && !key(cx, "findBarFocus"));

        // Open Results in Find Window hands the bar's search of its file to the Find tool window.
        run(cx, "find.inFile");
        let bar = workbench.read_with(cx, |w, _| w.find_bar()).expect("the bar is open");
        bar.update(cx, |bar, cx| bar.set_search("widget", cx));
        cx.run_until_parked();
        run(cx, "find.openInWindow");
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(FIND_TOOL_WINDOW)));
        let panel = workbench
            .read_with(cx, |w, _| w.tool_window_view(FIND_TOOL_WINDOW))
            .and_then(|view| view.downcast::<crate::find::view::FindPanel>().ok())
            .expect("the Find window holds the panel");
        settle(cx, |cx| panel.read_with(cx, |panel, _| panel.status() == crate::find::session::FindStatus::Done));
        assert_eq!(panel.read_with(cx, |panel, _| panel.results()), [("src/alpha.rs".to_owned(), vec![0, 2, 3])]);
    }

    /// While the bar searches, the editor marks the occurrences (the current one apart) and clears them
    /// when the bar closes; Replace All is one edit and one undo step; the bar's Add Selection for Next
    /// Occurrence adds a caret; Escape in the editor closes the bar.
    #[gpui_kit::test]
    async fn the_find_bar_marks_its_occurrences_replaces_in_one_step_and_closes_on_escape(cx: &mut TestAppContext) {
        use crate::editor::find_bar::FindBarMenu;
        use crate::editor::marks::{MarkKind, MarkLayer};
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let path = root.join("src/alpha.rs");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(path.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == path)));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        let marks = |cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| {
                e.marks(MarkLayer::Find).iter().map(|mark| (mark.range.clone(), mark.kind)).collect::<Vec<_>>()
            })
        };

        run(cx, "find.inFile");
        let bar = workbench.read_with(cx, |w, _| w.find_bar()).expect("the bar is open");
        cx.simulate_input("widget");
        cx.run_until_parked();
        // "pub struct Widget;\n\npub fn make_widget() -> Widget {\n    Widget\n}\n"
        let found = [11..17, 32..38, 44..50, 57..63];
        let kinds = |current: usize| {
            let kind =
                |index| if index == current { MarkKind::CurrentFindOccurrence } else { MarkKind::FindOccurrence };
            found.iter().cloned().enumerate().map(|(index, range)| (range, kind(index))).collect::<Vec<_>>()
        };
        assert_eq!(marks(cx), kinds(0));
        cx.simulate_keystrokes("enter");
        assert_eq!(marks(cx), kinds(1));
        cx.simulate_keystrokes("enter");
        assert_eq!(marks(cx), kinds(2));

        // The bar's Add Selection for Next Occurrence runs on its editor.
        bar.update(cx, |bar, cx| {
            bar.open_menu(FindBarMenu::More, cx);
            bar.choose_menu_item(3, cx);
        });
        cx.run_until_parked();
        assert_eq!(editor.read_with(cx, |e, _| e.selections().to_vec()), [(44, 50), (57, 63)]);

        // Replace All: one edit of the document of one change per occurrence, taken back by one undo. A
        // second editor of the file keeps its caret between two occurrences, where one change spanning
        // them all would have moved it.
        let opened = cx.update(|_, cx| {
            let path = path.clone();
            cx.global::<Backend>()
                .with_host(move |host| async move { ide_host::ops::doc::open(&host, "w1", path).await })
        });
        let info = opened.await.expect("the task completes").expect("the document opens again");
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        let sibling = cx.new(|cx| Editor::new(host, "w1", &info, cx).expect("a second editor"));
        sibling.update(cx, |e, cx| e.select(25, 25, cx));
        let version = editor.read_with(cx, |e, _| e.version());
        bar.update(cx, |bar, cx| bar.set_replacement("Gadget", cx));
        bar.update(cx, FindBar::replace_all);
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |e, _| e.text()),
            "pub struct Gadget;\n\npub fn make_Gadget() -> Gadget {\n    Gadget\n}\n"
        );
        assert_eq!(editor.read_with(cx, |e, _| e.version()), version + 1);
        assert_eq!(sibling.read_with(cx, |e, _| (e.text(), e.caret())), (editor.read_with(cx, |e, _| e.text()), 25));
        assert!(marks(cx).is_empty());
        run(cx, "edit.undo");
        assert_eq!(
            editor.read_with(cx, |e, _| e.text()),
            "pub struct Widget;\n\npub fn make_widget() -> Widget {\n    Widget\n}\n"
        );
        assert_eq!(marks(cx).len(), 4);

        // Escape in the editor closes the bar and clears its marks; another Escape is the editor's own.
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(workbench.read_with(cx, |w, _| w.find_bar()), None);
        assert!(marks(cx).is_empty());
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));
    }

    /// A single-file window has the bar too, over its editor.
    #[gpui_kit::test]
    async fn a_single_file_window_opens_the_find_bar_over_its_editor(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let file = folder.path().join("todo.txt");
        std::fs::write(&file, "milk\noat milk\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        cx.run_until_parked();
        cx.update(|window, cx| execute_action_or_throw("find.inFile", None, window, cx)).expect("it runs");
        cx.run_until_parked();
        let bar = workbench.read_with(cx, |w, _| w.find_bar()).expect("the bar is open");
        assert!(cx.update(|window, cx| bar.read(cx).has_focus(window, cx)));
        cx.simulate_input("milk");
        cx.run_until_parked();
        assert_eq!(bar.read_with(cx, |bar, _| bar.match_count()), 2);
        assert_eq!(editor.read_with(cx, |e, _| e.selection()), 0..4);
        cx.simulate_keystrokes("enter");
        assert_eq!(editor.read_with(cx, |e, _| e.selection()), 9..13);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(workbench.read_with(cx, |w, _| w.find_bar()), None);
        assert!(cx.update(|window, cx| editor.focus_handle(cx).is_focused(window)));
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    /// The status bar's items answer a click: the position opens Go to Line, the line separator
    /// and the encoding open their menus, whose rows change the file. What the backend refuses
    /// is told in a notification.
    #[gpui_kit::test]
    async fn the_status_bars_items_open_go_to_line_and_their_menus(cx: &mut TestAppContext) {
        use gpui_kit::Modifiers;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        let click = |cx: &mut VisualTestContext, item: &'static str| {
            cx.run_until_parked();
            let at = cx.debug_bounds(item).expect("the status bar item is drawn").center();
            cx.simulate_click(at, Modifiers::default());
        };
        let items = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, cx| w.status_items(cx));
        let menu = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.open_menu::<StatusCommand>()).expect("the item's menu is open")
        };
        assert_eq!(items(cx), ["1:1", "LF", "UTF-8"]);

        click(cx, "status:editor.caret");
        cx.simulate_input("2");
        cx.simulate_keystrokes("enter");
        assert_eq!(items(cx)[0], "2:1");

        // The file's own separator is marked; choosing another changes it and the file is unsaved.
        click(cx, "status:editor.lineEnding");
        let rows: Vec<(String, bool)> = menu(cx)
            .read_with(cx, |menu, _| menu.rows().iter().map(|row| (row.label.clone(), row.icon.is_some())).collect());
        assert_eq!(rows.iter().map(|(_, marked)| *marked).collect::<Vec<_>>(), [true, false, false]);
        assert!(rows[1].0.starts_with("CRLF"));
        cx.simulate_keystrokes("down enter");
        assert_eq!(items(cx)[1], "CRLF");
        assert!(editor.read_with(cx, |editor, _| editor.is_dirty()));

        // With unsaved changes the file cannot be reloaded in another encoding, but converted.
        click(cx, "status:editor.encoding");
        let rows: Vec<(String, bool)> =
            menu(cx).read_with(cx, |menu, _| menu.rows().iter().map(|row| (row.label.clone(), row.disabled)).collect());
        assert_eq!(rows[..2], [("Reload with Encoding".to_owned(), true), ("Convert to Encoding".to_owned(), false)]);
        // The first row cannot be chosen, so the selection starts on Convert; its list leaves out
        // the file's own encoding.
        cx.simulate_keystrokes("right");
        let offered: Vec<String> =
            menu(cx).read_with(cx, |menu, _| menu.rows().iter().map(|row| row.label.clone()).collect());
        assert!(!offered.contains(&"UTF-8".to_owned()) && offered[0] == "UTF-16LE", "{offered:?}");
        cx.simulate_keystrokes("enter");
        assert_eq!(items(cx)[2], "UTF-16LE");

        // A refusal by the backend is told.
        workbench.update(cx, |w, cx| w.run_status_command(StatusCommand::Reload("gbk".to_owned()), cx));
        let told = workbench.read_with(cx, |w, _| format!("{:?}", w.notifications()));
        assert!(told.contains("Cannot reload with encoding"), "{told}");
        assert_eq!(items(cx)[2], "UTF-16LE");
    }

    /// A raised notification shows as a balloon; closing the balloon keeps it in the history.
    /// The Notifications tool window lists the history, hides the balloons and marks everything
    /// read; Clear All empties it. "Don't Show Again" turns a group's balloons off in the settings.
    #[gpui_kit::test]
    async fn notifications_show_as_balloons_and_in_their_tool_window(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        use gpui_kit::Modifiers;
        let _home = start(cx);
        let (_folder, _root, workbench, cx) = search_project(cx);
        let raise = |cx: &mut VisualTestContext, level: NotificationLevel, title: &str| {
            workbench.update(cx, |w, cx| w.notify(NotificationRequest::new(level, title), cx));
            cx.run_until_parked();
        };
        let balloons = |cx: &mut VisualTestContext| -> Vec<String> {
            workbench
                .read_with(cx, |w, cx| w.balloons.read(cx).shown().iter().map(|entry| entry.title.clone()).collect())
        };
        let click = |cx: &mut VisualTestContext, selector: String| {
            cx.run_until_parked();
            let at = cx.debug_bounds(Box::leak(selector.into_boxed_str())).expect("it is drawn").center();
            cx.simulate_click(at, Modifiers::default());
            cx.run_until_parked();
        };

        raise(cx, NotificationLevel::Warning, "Disk is almost full");
        raise(cx, NotificationLevel::Error, "Build failed");
        assert_eq!(balloons(cx), ["Disk is almost full", "Build failed"]);
        assert_eq!(workbench.read_with(cx, |w, _| w.notifications().unread_count()), 2);

        // The close button takes the balloon away; the notification stays in the history.
        let first = workbench.read_with(cx, |w, cx| w.balloons.read(cx).shown()[0].id);
        click(cx, format!("balloon-{first}-close"));
        assert_eq!(balloons(cx), ["Build failed"]);
        assert_eq!(workbench.read_with(cx, |w, _| w.notifications().items().len()), 2);

        // The tool window lists both, newest first; while it shows there are no balloons and
        // nothing is unread.
        cx.update(|window, cx| execute_action_or_throw("view.notifications", None, window, cx)).expect("it runs");
        cx.run_until_parked();
        let list = workbench
            .read_with(cx, |w, _| w.tool_window_view(NOTIFICATIONS_TOOL_WINDOW))
            .and_then(|view| view.downcast::<NotificationsView>().ok())
            .expect("the Notifications window holds the list");
        let listed: Vec<String> =
            list.read_with(cx, |list, _| list.entries().iter().map(|entry| entry.title.clone()).collect());
        assert_eq!(listed, ["Build failed", "Disk is almost full"]);
        assert!(balloons(cx).is_empty());
        assert_eq!(workbench.read_with(cx, |w, _| w.notifications().unread_count()), 0);

        click(cx, "notifications-clear-all".to_owned());
        assert!(list.read_with(cx, |list, _| list.entries().is_empty()));
        assert!(workbench.read_with(cx, |w, _| w.notifications().items().is_empty()));

        // With the window hidden again a new notification is a balloon; its Don't Show Again
        // stores the choice, and the next one of its group does not pop up.
        cx.update(|window, cx| execute_action_or_throw("toolwindow.notifications", None, window, cx)).expect("it runs");
        cx.run_until_parked();
        assert!(!workbench.read_with(cx, |w, _| w.is_tool_window_visible(NOTIFICATIONS_TOOL_WINDOW)));
        raise(cx, NotificationLevel::Info, "Indexing finished");
        assert_eq!(balloons(cx), ["Indexing finished"]);
        let entry = workbench.read_with(cx, |w, cx| w.balloons.read(cx).shown()[0].clone());
        let dont_show = entry
            .actions
            .iter()
            .position(|action| matches!(action, EntryAction::DontShowAgain { .. }))
            .expect("an information balloon offers it");
        click(cx, format!("balloon-{}-action-{dont_show}", entry.id));
        assert!(balloons(cx).is_empty());
        raise(cx, NotificationLevel::Info, "Indexing finished again");
        assert!(balloons(cx).is_empty(), "the group's balloons are off");
        assert_eq!(workbench.read_with(cx, |w, _| w.notifications().items().len()), 2, "they are still recorded");
    }

    /// A window with nothing open shows the Welcome screen. A project that was opened and closed
    /// is listed there; Enter or a click on its row opens it again, and removing it from the list
    /// forgets it.
    #[gpui_kit::test]
    async fn a_window_with_nothing_open_welcomes_with_the_recent_projects(cx: &mut TestAppContext) {
        use gpui_kit::Modifiers;
        use welcome_model::WelcomeCommand;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        let name = root.file_name().expect("a folder name").to_string_lossy().into_owned();
        // A decided folder reopens from the Welcome screen without the trust question.
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        let welcome = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.welcome().cloned());
        let names = |cx: &mut VisualTestContext| -> Vec<String> {
            let view = welcome(cx).expect("the Welcome screen shows");
            view.read_with(cx, |view, _| view.shown_projects().into_iter().map(|project| project.name).collect())
        };
        let close = |cx: &mut VisualTestContext| {
            workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
            cx.run_until_parked();
        };
        let opened = |cx: &mut VisualTestContext| {
            settle(cx, |cx| workbench.read_with(cx, |w, cx| w.project_root(cx).as_deref() == Some(&*root)));
            assert!(welcome(cx).is_none(), "a project replaces the Welcome screen");
        };

        // Nothing has been opened yet: the page that offers to open one.
        cx.run_until_parked();
        let first = welcome(cx).expect("the Welcome screen shows");
        assert_eq!(first.read_with(cx, |view, _| view.empty_text()), Some("Welcome to OpenIDEA"));
        assert_eq!(
            first.read_with(cx, |view, _| view.version().map(str::to_owned)),
            Some(env!("CARGO_PKG_VERSION").into())
        );
        assert!(cx.debug_bounds("welcome-brand").is_some(), "the Welcome screen is drawn");

        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        opened(cx);

        // Closed, the project is the recent one; the keyboard is in the search, and Enter opens it.
        close(cx);
        assert_eq!(names(cx), [name]);
        let field = welcome(cx).expect("the Welcome screen shows").read_with(cx, |view, _| view.field().clone());
        let searching = |cx: &mut VisualTestContext| cx.update(|window, cx| field.focus_handle(cx).is_focused(window));
        assert!(searching(cx));
        // Recent Projects puts the keyboard back in the search.
        cx.update(|window, cx| window.blur(cx));
        assert!(!searching(cx));
        cx.update(|window, cx| {
            crate::platform::actions::execute_action_or_throw("welcome.recentProjects", None, window, cx)
        })
        .expect("it runs");
        assert!(searching(cx));
        cx.simulate_keystrokes("enter");
        opened(cx);

        // A click on its row opens it too.
        close(cx);
        let row = cx.debug_bounds("welcome-row-0").expect("the row is drawn").center();
        cx.simulate_click(row, Modifiers::default());
        opened(cx);

        // Removing it from the list, once confirmed, forgets it.
        close(cx);
        let view = welcome(cx).expect("the Welcome screen shows");
        let path = root.to_string_lossy().into_owned();
        view.update_in(cx, |view, window, cx| view.run(WelcomeCommand::AskRemove(path), window, cx));
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert!(names(cx).is_empty());
        let config = cx.update(|_, cx| cx.global::<Backend>().dirs.config.clone());
        assert!(ide_workspace::RecentProjects::load(&config).list().is_empty(), "the project is forgotten");
        assert_eq!(
            welcome(cx).map(|view| view.read_with(cx, |view, _| view.empty_text())),
            Some(Some("Welcome to OpenIDEA"))
        );
    }

    /// Run › Run starts the selected configuration of the project: its output shows in the Run tool
    /// window and the session ends with the process's exit code; Run › Stop ends a long-running one.
    #[gpui_kit::test]
    async fn a_run_configuration_runs_into_the_run_tool_window_and_stop_ends_a_long_running_one(
        cx: &mut TestAppContext,
    ) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::run::configs::blank_config;
        use crate::run::store::SessionStatus;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        let program = |id: &str, command: &str, arg: &str| ide_run::RunConfig {
            command: command.to_owned(),
            args: vec![arg.to_owned()],
            ..blank_config(id, id, "program")
        };
        let configs = [program("hello", "/bin/echo", "hello"), program("nap", "/bin/sleep", "30")];
        ide_run::save_user_configs(&root, &configs).expect("the configurations are saved");
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let driver = workbench.read_with(cx, |w, _| w.run_driver().cloned()).expect("the project runs things");
        let store = driver.read_with(cx, |driver, _| driver.store().clone());
        // Processes and their timers: real time passes, and so does the executor's clock.
        let long = |cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool| {
            for _ in 0..3000 {
                cx.executor().advance_clock(Duration::from_millis(20));
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("{what} did not happen within thirty seconds");
        };
        long(cx, "listing the configurations", &|cx| store.read_with(cx, |store, _| store.loaded));
        let listed: Vec<String> =
            store.read_with(cx, |store, _| store.configs().iter().map(|c| c.id.clone()).collect());
        assert_eq!(listed, ["hello", "nap"]);
        assert!(!workbench.read_with(cx, |w, _| w.is_tool_window_visible(RUN_TOOL_WINDOW)));

        store.update(cx, |store, _| store.select_config(Some("hello")));
        cx.update(|window, cx| execute_action_or_throw("run.run", None, window, cx)).expect("it runs");
        let session = |cx: &mut VisualTestContext, name: &str| {
            store.read_with(cx, |store, _| store.sessions.iter().find(|s| s.config.name == name).cloned())
        };
        long(cx, "the exit of echo", &|cx| session(cx, "hello").is_some_and(|s| !s.is_live()));
        let hello = session(cx, "hello").expect("a tab");
        assert_eq!((hello.status, hello.exit_code, hello.stopped), (SessionStatus::Finished, Some(0), false));
        // Settings › Run › Show the Run tool window when a process starts.
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(RUN_TOOL_WINDOW)));
        let view = workbench
            .read_with(cx, |w, _| w.tool_window_view(RUN_TOOL_WINDOW))
            .and_then(|view| view.downcast::<RunView>().ok())
            .expect("the Run tool window holds its view");
        assert_eq!(view.read_with(cx, |view, cx| view.tab_titles(cx)), ["hello"]);
        let lines = view.read_with(cx, |view, cx| view.lines(cx));
        assert!(lines.iter().any(|line| line == "hello"), "{lines:?}");
        assert_eq!(view.read_with(cx, |view, cx| view.tab_icons(cx)), ["file-binary"]);

        // With the keyboard in the console's find bar the Run tool window is the active one: Rerun
        // restarts its tab.
        cx.update(|window, cx| {
            let focus = view.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("cmd-f");
        workbench.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.find_open()));
        let active = cx.update(|window, cx| {
            let context = crate::platform::context::window_context(window, cx);
            matches!(context.get("activeToolWindow"), Some(crate::platform::context::ContextValue::Text(id)) if &**id == "run")
        });
        assert!(active, "the Run tool window is the active one");
        cx.update(|window, cx| execute_action_or_throw("run.rerun", None, window, cx)).expect("it reruns");
        let first = hello.started_at;
        long(cx, "the rerun of echo", &|cx| {
            session(cx, "hello").is_some_and(|s| s.started_at > first && s.status == SessionStatus::Finished)
        });
        assert_eq!(view.read_with(cx, |view, cx| view.tab_titles(cx)), ["hello"], "the same tab");
        cx.simulate_keystrokes("escape");

        store.update(cx, |store, _| store.select_config(Some("nap")));
        cx.update(|window, cx| execute_action_or_throw("run.run", None, window, cx)).expect("it runs");
        long(cx, "the start of sleep", &|cx| session(cx, "nap").is_some_and(|s| s.status == SessionStatus::Running));
        assert_eq!(view.read_with(cx, |view, cx| view.tab_icons(cx)), ["file-binary", "run"]);
        // Outside the Run tool window Stop reaches the only running process.
        cx.update(|window, cx| execute_action_or_throw("run.stop", None, window, cx)).expect("it runs");
        long(cx, "the end of sleep", &|cx| session(cx, "nap").is_some_and(|s| !s.is_live()));
        let nap = session(cx, "nap").expect("a tab");
        assert_eq!((nap.status, nap.stopped), (SessionStatus::Finished, true));
        assert_eq!(view.read_with(cx, |view, cx| view.tab_icons(cx)), ["file-binary", "file-binary"]);
    }

    #[gpui_kit::test]
    async fn a_run_configuration_added_to_services_runs_from_its_row_into_the_detail_pane_and_stop_ends_a_service(
        cx: &mut TestAppContext,
    ) {
        use crate::platform::actions::execute_action_or_throw;
        use crate::run::configs::blank_config;
        use crate::run::store::SessionStatus;
        use crate::services::view::ServicesView;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        let program = |id: &str, command: &str, arg: &str| ide_run::RunConfig {
            command: command.to_owned(),
            args: vec![arg.to_owned()],
            ..blank_config(id, id, "program")
        };
        let configs = [program("hello", "/bin/echo", "hello"), program("nap", "/bin/sleep", "30")];
        ide_run::save_user_configs(&root, &configs).expect("the configurations are saved");
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let driver = workbench.read_with(cx, |w, _| w.run_driver().cloned()).expect("the project runs things");
        let store = driver.read_with(cx, |driver, _| driver.store().clone());
        let long = |cx: &mut VisualTestContext, what: &str, done: &dyn Fn(&mut VisualTestContext) -> bool| {
            for _ in 0..3000 {
                cx.executor().advance_clock(Duration::from_millis(20));
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("{what} did not happen within thirty seconds");
        };
        long(cx, "listing the configurations", &|cx| store.read_with(cx, |store, _| store.loaded));

        // "Program" is not a default type: Add Service lists its configurations in the window.
        cx.update(|window, cx| execute_action_or_throw("services.show", None, window, cx)).expect("it shows");
        let arg = serde_json::Value::from("program");
        cx.update(|window, cx| execute_action_or_throw("run.services.addType", Some(&arg), window, cx))
            .expect("the type is added");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(SERVICES_TOOL_WINDOW)));
        let view = workbench
            .read_with(cx, |w, _| w.tool_window_view(SERVICES_TOOL_WINDOW))
            .and_then(|view| view.downcast::<ServicesView>().ok())
            .expect("the Services tool window holds its view");
        let row = |cx: &mut VisualTestContext, key: &str| {
            view.read_with(cx, |view, _| view.rows().into_iter().find(|row| row.key == key))
        };
        let (hello_key, nap_key) = ("run:config:hello", "run:config:nap");
        assert_eq!(row(cx, "run:type:program").map(|row| row.label), Some("Program".to_owned()));
        assert_eq!(row(cx, hello_key).map(|row| row.label), Some("hello".to_owned()));
        assert!(row(cx, nap_key).is_some());

        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.run_until_parked();
            let bounds = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn"));
            cx.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
            cx.run_until_parked();
        };
        let session = |cx: &mut VisualTestContext, name: &str| {
            store.read_with(cx, |store, _| store.sessions.iter().find(|s| s.config.name == name).cloned())
        };
        let detail_lines = |cx: &mut VisualTestContext| {
            let pane = workbench.read_with(cx, |w, _| w.services.as_ref().and_then(|s| s.detail().cloned()));
            pane.map(|pane| pane.read_with(cx, |pane, cx| pane.lines(cx))).unwrap_or_default()
        };

        // Run on the selected row runs it in place: the output shows in the detail pane.
        click(cx, "services-row:run:config:hello");
        assert_eq!(detail_lines(cx), ["hello", "Program", "/bin/echo hello"]);
        click(cx, "services-action:run");
        long(cx, "the exit of echo", &|cx| session(cx, "hello").is_some_and(|s| !s.is_live()));
        let hello = session(cx, "hello").expect("a session");
        assert_eq!((hello.status, hello.exit_code), (SessionStatus::Finished, Some(0)));
        long(cx, "the output in the detail pane", &|cx| detail_lines(cx).iter().any(|line| line == "hello"));
        assert_eq!(row(cx, hello_key).and_then(|row| row.detail), Some("Finished".to_owned()));
        assert!(!workbench.read_with(cx, |w, _| w.is_tool_window_visible(RUN_TOOL_WINDOW)), "it ran in place");

        // Stop on a running service ends it.
        click(cx, "services-row:run:config:nap");
        click(cx, "services-action:run");
        long(cx, "the start of sleep", &|cx| session(cx, "nap").is_some_and(|s| s.status == SessionStatus::Running));
        assert_eq!(row(cx, nap_key).and_then(|row| row.detail), Some("Running".to_owned()));
        click(cx, "services-action:stop");
        long(cx, "the end of sleep", &|cx| session(cx, "nap").is_some_and(|s| !s.is_live()));
        let nap = session(cx, "nap").expect("a session");
        assert_eq!((nap.status, nap.stopped), (SessionStatus::Finished, true));
        assert_eq!(row(cx, nap_key).and_then(|row| row.detail), Some("Stopped".to_owned()));
    }

    /// In Restricted Mode Run starts nothing: the configuration stays listed but the backend refuses
    /// to run it, so the program never runs. Once the project is trusted it runs.
    #[gpui_kit::test]
    async fn a_project_in_restricted_mode_runs_no_configuration_until_it_is_trusted(cx: &mut TestAppContext) {
        let marker = tempfile::tempdir().expect("a temporary folder");
        let ran = marker.path().join("ran.txt");
        let touch = program("touch", "/usr/bin/touch", &ran.to_string_lossy());
        let (workbench, cx, _folders, _root, store) = run_project(cx, vec![touch]);
        let restricted = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, cx| w.project_restricted(cx));
        let listed = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| store.configs().iter().map(|c| c.name.clone()).collect::<Vec<_>>())
        };
        run_action(cx, "project.revokeTrust", None);
        wait_long(cx, "Restricted Mode", restricted);
        assert_eq!(listed(cx), ["touch"]);
        run_action(cx, "run.run", None);
        for _ in 0..50 {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(store.read_with(cx, |store, _| store.sessions.iter().all(|s| !s.is_live())), "nothing runs");
        assert!(!ran.exists(), "the program did not run");
        let notice = notice_titled(&workbench, "Error running 'touch'", cx).expect("the refusal is reported");
        assert!(notice.body.as_deref().is_some_and(|body| body.contains("Restricted Mode")), "{:?}", notice.body);

        run_action(cx, "project.trust", None);
        wait_long(cx, "the trust", |cx| !restricted(cx));
        run_action(cx, "run.run", None);
        wait_long(cx, "the program", |_| ran.exists());
    }

    /// The Run console of a configuration that emulates a terminal tells its process the size it
    /// fits: `stty size` in the program reads the rows and columns the console reports, and the
    /// new ones once the window is resized.
    #[gpui_kit::test]
    async fn a_run_in_a_terminal_console_is_told_the_consoles_size_and_follows_a_resize(cx: &mut TestAppContext) {
        let mut sizes = ide_run::RunConfig {
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "while true; do stty size; sleep 0.1; done".to_owned()],
            ..crate::run::configs::blank_config("sizes", "sizes", "program")
        };
        sizes.extras.emulate_terminal = true;
        let (workbench, cx, _folders, _root, store) = run_project(cx, vec![sizes]);
        run_action(cx, "run.run", None);
        wait_long(cx, "the Run tool window", |cx| {
            workbench.read_with(cx, |w, _| w.tool_window_view(RUN_TOOL_WINDOW)).is_some()
        });
        let view = workbench
            .read_with(cx, |w, _| w.tool_window_view(RUN_TOOL_WINDOW))
            .and_then(|view| view.downcast::<RunView>().ok())
            .expect("the Run tool window holds its view");
        let told = |cx: &mut VisualTestContext| {
            let Some((cols, rows)) = view.read_with(cx, |view, _| view.terminal_size()) else { return false };
            let line = format!("{rows} {cols}");
            view.read_with(cx, |view, cx| view.lines(cx)).iter().any(|shown| shown.trim() == line)
        };
        wait_long(cx, "the program to read the console's size", told);
        let before = view.read_with(cx, |view, _| view.terminal_size());

        cx.simulate_resize(gpui_kit::size(gpui_kit::px(900.0), gpui_kit::px(640.0)));
        wait_long(cx, "a new size", |cx| view.read_with(cx, |view, _| view.terminal_size()) != before);
        wait_long(cx, "the program to read the new size", told);
        let session = store.read_with(cx, |store, _| store.sessions.first().map(|s| s.id.clone())).expect("a session");
        let driver = workbench.read_with(cx, |w, _| w.run_driver().cloned()).expect("the project runs things");
        driver.update(cx, |driver, cx| driver.stop(&session, cx));
        wait_long(cx, "the end of the program", |cx| {
            store.read_with(cx, |store, _| store.sessions.iter().all(|s| !s.is_live()))
        });
    }

    /// A program configuration that runs `command arg`.
    fn program(id: &str, command: &str, arg: &str) -> ide_run::RunConfig {
        ide_run::RunConfig {
            command: command.to_owned(),
            args: vec![arg.to_owned()],
            ..crate::run::configs::blank_config(id, id, "program")
        }
    }

    /// A trusted project whose saved configurations are `configs`, open in a window whose run
    /// configurations are listed: the window, the project's folder (kept while it lives) and its root,
    /// and its run store.
    fn run_project(
        cx: &mut TestAppContext,
        configs: Vec<ide_run::RunConfig>,
    ) -> (Entity<Workbench>, &mut VisualTestContext, [tempfile::TempDir; 2], PathBuf, Entity<RunStore>) {
        let home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        ide_run::save_user_configs(&root, &configs).expect("the configurations are saved");
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let driver = workbench.read_with(cx, |w, _| w.run_driver().cloned()).expect("the project runs things");
        let store = driver.read_with(cx, |driver, _| driver.store().clone());
        wait_long(cx, "listing the configurations", |cx| store.read_with(cx, |store, _| store.loaded));
        (workbench, cx, [home, folder], root, store)
    }

    /// Waits for `done` while processes run and their timers fire: real time passes, and so does the
    /// executor's clock.
    pub(super) fn wait_long(cx: &mut VisualTestContext, what: &str, done: impl Fn(&mut VisualTestContext) -> bool) {
        let started = std::time::Instant::now();
        let poll = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            let reached = done(cx);
            if !reached {
                std::thread::sleep(Duration::from_millis(10));
            }
            reached
        };
        for _ in 0..3000 {
            if poll(cx) {
                return;
            }
        }
        // The state when the wait runs out, and whether more time would have been enough.
        let report = window_vcs_report(cx);
        let ran_out = started.elapsed();
        let later = (0..3000).find(|_| poll(cx)).map(|_| started.elapsed());
        panic!(
            "{what} did not happen within thirty seconds (ran out after {ran_out:?}; reached later: {later:?})\n\
             state when it ran out: {report}\nstate now: {}",
            window_vcs_report(cx)
        );
    }

    /// The repository state of the window's workbench, when its root is one.
    fn window_vcs_report(cx: &mut VisualTestContext) -> String {
        let root = cx.update(|window, _| window.root::<Workbench>().flatten());
        root.map_or_else(
            || "the window shows no workbench".to_owned(),
            |w| {
                w.read_with(cx, |w, cx| {
                    let merge = w.merge_view().map(|view| {
                        let view = view.read(cx);
                        format!("loaded={} modified={}", view.text().is_some(), view.is_modified())
                    });
                    let notices: Vec<&str> = w.notifications().items().iter().map(|n| n.title.as_str()).collect();
                    let tree = w.project_tree().map(|tree| tree.read(cx).trace_report());
                    let runs: Vec<String> = w.run_driver().map_or_else(Vec::new, |driver| {
                        let store = driver.read(cx).store().read(cx);
                        store
                            .sessions
                            .iter()
                            .map(|run| {
                                let lines = store.console(&run.id).map(|console| console.lines()).unwrap_or_default();
                                let last: Vec<&str> =
                                    lines.iter().rev().take(4).rev().map(|line| line.text.as_str()).collect();
                                format!(
                                    "  {} process={:?} {:?} exit={:?} stopped={} started={:.0} ended={:?} \
                                     last={last:?}",
                                    run.config.name,
                                    run.run_id,
                                    run.status,
                                    run.exit_code,
                                    run.stopped,
                                    run.started_at,
                                    run.ended_at
                                )
                            })
                            .collect()
                    });
                    format!(
                        "FSEvents streams of this process: {}\n\
                         merge editor={merge:?} question={} notices={notices:?} {}\n\
                         project tree:\n{}\nruns:\n{}",
                        ide_vfs::fsevent_streams(),
                        w.vcs_prompt().is_some(),
                        w.vcs_report(),
                        tree.unwrap_or_default(),
                        runs.join("\n")
                    )
                })
            },
        )
    }

    pub(super) fn click_on(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
    }

    pub(super) fn run_action(cx: &mut VisualTestContext, id: &str, arg: Option<serde_json::Value>) {
        cx.update(|window, cx| crate::platform::actions::execute_action_or_throw(id, arg.as_ref(), window, cx))
            .unwrap_or_else(|error| panic!("{id} runs: {error}"));
        cx.run_until_parked();
    }

    /// An ACP agent for tests: a Python script speaking newline-delimited JSON-RPC on stdio. A prompt
    /// is answered with "Hello from the fake agent" in two chunks; one mentioning "tool" reports the
    /// finished tool call "Run cargo test" and says "The tests pass"; one mentioning "permission" first
    /// asks to run `cargo test` and then says which option it was given; one mentioning "wait" says
    /// hello and works until it is cancelled. It ends with its input.
    const FAKE_AGENT: &str = r#"import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

def answer(request, result):
    send({"jsonrpc": "2.0", "id": request, "result": result})

def say(session, text):
    update = {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}})

asked = {}
working = None
next_request = 1000
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    method = message.get("method")
    params = message.get("params") or {}
    if method is None:
        turn = asked.pop(message.get("id"), None)
        if turn is not None:
            outcome = (message.get("result") or {}).get("outcome") or {}
            say(turn[1], "You chose " + outcome.get("optionId", "nothing"))
            answer(turn[0], {"stopReason": "end_turn"})
        continue
    if method == "session/cancel":
        if working is not None:
            answer(working, {"stopReason": "cancelled"})
            working = None
        continue
    if "id" not in message:
        continue
    request = message["id"]
    if method == "initialize":
        answer(request, {"protocolVersion": 1, "agentCapabilities": {"loadSession": False}, "authMethods": []})
    elif method == "session/new":
        answer(request, {"sessionId": "fake-session"})
    elif method == "session/prompt":
        session = params.get("sessionId", "fake-session")
        blocks = params.get("prompt") or []
        text = " ".join(block.get("text", "") for block in blocks if block.get("type") == "text")
        if "tool" in text:
            call = {"sessionUpdate": "tool_call", "toolCallId": "tests", "title": "Run cargo test", "kind": "execute",
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "3 passed"}}]}
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": call}})
            say(session, "The tests pass")
            answer(request, {"stopReason": "end_turn"})
        elif "permission" in text:
            say(session, "I need to run the tests.")
            next_request += 1
            asked[next_request] = (request, session)
            send({"jsonrpc": "2.0", "id": next_request, "method": "session/request_permission", "params": {
                "sessionId": session,
                "toolCall": {"toolCallId": "run-tests", "title": "Run cargo test", "kind": "execute"},
                "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                            {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}})
        else:
            say(session, "Hello ")
            say(session, "from the fake agent")
            if "wait" in text:
                working = request
            else:
                answer(request, {"stopReason": "end_turn"})
    else:
        send({"jsonrpc": "2.0", "id": request, "error": {"code": -32601, "message": method}})
"#;

    /// The API key the fake agent starts with: never shown, copied or printed.
    const FAKE_AGENT_KEY: &str = "sk-fake-agent-secret-0123456789";

    type AgentsFixture<'a> =
        (tempfile::TempDir, Entity<Workbench>, Entity<crate::agents::store::AgentsStore>, &'a mut VisualTestContext);

    /// Installs the fake agent as the agent `fake` of the configuration under `home`, as a user writes
    /// `<config>/agents/<id>.toml`, and makes it the New Task page's agent: no test reaches an agent
    /// installed on the machine.
    fn install_fake_agent(home: &std::path::Path, cx: &mut TestAppContext) {
        let script = home.join("fake_agent.py");
        std::fs::write(&script, FAKE_AGENT).expect("the agent script is written");
        let agents = AppDirs::under(home).config.join("agents");
        std::fs::create_dir_all(&agents).expect("the agents folder is made");
        let file = format!(
            "name = \"Fake Agent\"\ncommand = \"python3\"\nargs = [{:?}]\n\n[env]\nFAKE_AGENT_API_KEY = {:?}\n",
            script.to_string_lossy(),
            FAKE_AGENT_KEY,
        );
        std::fs::write(agents.join("fake.toml"), file).expect("the agent file is written");
        let draft = r#"{"provider":"fake"}"#.to_owned();
        cx.update(|cx| cx.global_mut::<UiStorage>().set(crate::agents::store::DRAFT_KEY, draft));
    }

    /// A trusted project in an active window, with the fake agent listed among its agents.
    fn agents_project<'a>(cx: &'a mut TestAppContext, home: &std::path::Path) -> AgentsFixture<'a> {
        install_fake_agent(home, cx);
        let (folder, _root, workbench, cx) = trusted_project(cx, &[("README.md", "A project for agents.\n")]);
        cx.update(|window, _| window.activate_window());
        let store = workbench.read_with(cx, |w, _| w.agents_store()).expect("the project has its agents");
        wait_long(cx, "the agents to be listed", |cx| {
            store.read_with(cx, |store, _| store.loaded && store.provider("fake").is_some_and(|p| p.installed))
        });
        (folder, workbench, store, cx)
    }

    /// Starts a task with `prompt` from the New Task page of the Agents tool window (New Agent Task
    /// with the prompt as its argument, then Start) and waits for its conversation to dock there;
    /// returns the task's id.
    fn start_agent_task(
        workbench: &Entity<Workbench>,
        store: &Entity<crate::agents::store::AgentsStore>,
        prompt: &str,
        cx: &mut VisualTestContext,
    ) -> String {
        run_action(cx, "agents.newTask", Some(serde_json::json!(prompt)));
        wait_long(cx, "the New Task page", |cx| workbench.read_with(cx, |w, _| w.agents_home_page().is_some()));
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(crate::agents::model::TOOL_WINDOW)));
        let page = workbench.read_with(cx, |w, _| w.agents_home_page()).expect("the New Task page shows");
        assert_eq!(page.read_with(cx, |page, cx| page.field().read(cx).text().to_owned()), prompt);
        assert!(page.read_with(cx, |page, cx| page.can_start(cx)));
        // Only the fake agent may ever start.
        assert_eq!(store.read_with(cx, |store, _| store.draft.provider.clone()), "fake");
        click_on(cx, "agents-home-start");
        wait_long(cx, "the task's conversation", |cx| {
            workbench.read_with(cx, |w, _| w.docked_agent_session().is_some())
        });
        assert!(page.read_with(cx, |page, _| page.error().is_none()));
        let tasks: Vec<String> = store.read_with(cx, |store, _| store.tasks().iter().map(|t| t.id.clone()).collect());
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        tasks[0].clone()
    }

    /// The docked conversation, a line per row (`SessionView::lines`).
    fn agent_conversation(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Vec<String> {
        let Some((session, _)) = workbench.read_with(cx, |w, _| w.docked_agent_session()) else { return Vec::new() };
        session.update(cx, |session, cx| session.lines(cx))
    }

    fn agent_status(
        store: &Entity<crate::agents::store::AgentsStore>,
        task: &str,
        cx: &mut VisualTestContext,
    ) -> Option<ide_agent::AgentStatus> {
        store.read_with(cx, |store, _| store.task(task).map(|t| t.status))
    }

    fn agent_working(
        store: &Entity<crate::agents::store::AgentsStore>,
        task: &str,
        cx: &mut VisualTestContext,
    ) -> bool {
        use ide_agent::AgentStatus;
        matches!(agent_status(store, task, cx), Some(AgentStatus::Running | AgentStatus::Starting))
    }

    /// A test build refuses to start an agent its own config folder does not declare: a task with the
    /// default agent of a new task fails with the refusal on the New Task page and reaches neither the
    /// backend nor the seam standing in for it; the fake agent of the test's home passes the same guard
    /// and reaches the seam.
    #[gpui_kit::test]
    async fn a_task_with_the_default_agent_is_refused_in_a_test_build_and_starts_no_process(cx: &mut TestAppContext) {
        use super::agents::NewTaskEvent;
        use super::agents::spawn_guard::{AgentSpawnSeam, REFUSED};
        let home = start(cx);
        let (_folder, workbench, store, cx) = agents_project(cx, home.path());
        let seam = AgentSpawnSeam::default();
        cx.update(|_, cx| cx.set_global(seam.clone()));
        let started = |seam: &AgentSpawnSeam| seam.started.lock().map(|started| started.clone()).unwrap_or_default();

        run_action(cx, "agents.newTask", Some(serde_json::json!("Say hello")));
        wait_long(cx, "the New Task page", |cx| workbench.read_with(cx, |w, _| w.agents_home_page().is_some()));
        let page = workbench.read_with(cx, |w, _| w.agents_home_page()).expect("the New Task page shows");
        let default = crate::agents::store::Draft::default().provider;
        assert_eq!(default, "claude");
        store.update(cx, |store, _| store.draft.provider.clone_from(&default));
        page.update(cx, |_, cx| cx.emit(NewTaskEvent::Start));
        wait_long(cx, "the refusal", |cx| page.read_with(cx, |page, _| page.error().is_some()));
        let error = page.read_with(cx, |page, _| page.error().map(str::to_owned)).unwrap_or_default();
        assert!(error.starts_with(&format!("{REFUSED} `claude`")), "{error}");
        assert_eq!(started(&seam), Vec::<String>::new(), "nothing reached the seam");
        assert!(store.read_with(cx, |store, _| store.tasks().is_empty()), "no task exists");
        let backend = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        let tasks = ide_host::ops::agent::subscribe(&backend, "w1", std::sync::Arc::new(|_| {}));
        assert_eq!(tasks.map(|tasks| tasks.len()).ok(), Some(0), "the backend started no task");

        store.update(cx, |store, _| store.draft.provider = "fake".to_owned());
        page.update(cx, |_, cx| cx.emit(NewTaskEvent::Start));
        wait_long(cx, "the fake agent reaches the seam", |_| !started(&seam).is_empty());
        assert_eq!(started(&seam), ["fake"]);
        assert!(store.read_with(cx, |store, _| store.tasks().is_empty()), "the seam started nothing");
    }

    /// A task started from the Agents tool window shows the user's message and the agent's reply while
    /// the agent still works on the turn, with Stop in its composer; Stop cancels the turn, and the
    /// task list names the task with its agent.
    #[gpui_kit::test]
    async fn a_task_started_from_the_tool_window_streams_its_reply_and_stop_cancels_the_turn(cx: &mut TestAppContext) {
        let home = start(cx);
        let (_folder, workbench, store, cx) = agents_project(cx, home.path());
        let task = start_agent_task(&workbench, &store, "Say hello and wait", cx);
        wait_long(cx, "the agent's reply", |cx| {
            agent_conversation(&workbench, cx).iter().any(|line| line.starts_with("agent: Hello from the fake agent"))
        });
        let lines = agent_conversation(&workbench, cx);
        assert!(lines.iter().any(|line| line.starts_with("you: Say hello and wait")), "{lines:?}");
        assert!(agent_working(&store, &task, cx));
        let (_, composer) = workbench.read_with(cx, |w, _| w.docked_agent_session()).expect("the conversation docks");
        assert!(composer.read_with(cx, |composer, cx| composer.buttons(cx)).contains(&"Stop".to_owned()));

        click_on(cx, "composer-stop");
        wait_long(cx, "the turn to stop", |cx| !agent_working(&store, &task, cx));
        assert!(!composer.read_with(cx, |composer, cx| composer.buttons(cx)).contains(&"Stop".to_owned()));
        let list = workbench.read_with(cx, |w, _| w.agents_list()).expect("the tool window has its view");
        let rows = list.read_with(cx, |list, cx| list.rows(cx));
        assert!(rows.iter().any(|row| row.contains(" | Say hello and wait | Fake Agent | ")), "{rows:?}");
        workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
    }

    /// New Agent Task (⌘⇧I) and the debugger's keys need a project: a window without one runs none.
    #[gpui_kit::test]
    async fn the_new_agent_task_and_debugger_shortcuts_need_a_project(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        for key in ["cmd-shift-i", "ctrl-d", "cmd-f8", "cmd-shift-f8", "cmd-alt-shift-f8", "cmd-5", "f8", "f9"] {
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
        }
        assert!(workbench.read_with(cx, |w, _| {
            w.agents_store().is_none() && w.debug_driver().is_none() && w.tool_windows().is_none()
        }));
        for id in ["run.debug", "run.debugConfig"] {
            let run = cx.update(|window, cx| crate::platform::actions::execute_action_or_throw(id, None, window, cx));
            assert!(run.is_err(), "{id} runs without a project");
        }
        let run = cx
            .update(|window, cx| crate::platform::actions::execute_action_or_throw("agents.newTask", None, window, cx));
        assert!(run.is_err(), "agents.newTask runs without a project");
    }

    /// ⌘⇧I opens the New Task page from a text editor; the agent's API key is never shown, copied or
    /// printed; ⌘F in a conversation is not the editor's Find; Send to Agent, Quote, Copy, Rename,
    /// Jump to Latest and every placement action keep the conversation on screen in the tool window.
    #[gpui_kit::test]
    async fn the_agents_actions_keep_the_conversation_docked_and_never_show_the_agents_key(cx: &mut TestAppContext) {
        use crate::agents::model::TOOL_WINDOW;
        use crate::agents::store::SessionLocation;
        use crate::vcs::prompt_dialogs::PromptDialog;
        let home = start(cx);
        let (folder, workbench, store, cx) = agents_project(cx, home.path());
        let root = folder.path().canonicalize().expect("the folder exists");
        let readme = root.join("README.md");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(readme.clone(), window, cx));
        wait_long(cx, "the editor", |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-i");
        wait_long(cx, "the New Task page", |cx| workbench.read_with(cx, |w, _| w.agents_home_page().is_some()));
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(TOOL_WINDOW)));

        let task = start_agent_task(&workbench, &store, "Say hello", cx);
        wait_long(cx, "the reply", |cx| {
            !agent_working(&store, &task, cx)
                && agent_conversation(&workbench, cx).iter().any(|line| line.starts_with("agent: Hello"))
        });
        // The agent started with its key, and nothing on screen or in a `Debug` output holds it.
        let env = store.read_with(cx, |store, _| store.provider("fake").map(|p| p.env.clone()));
        assert_eq!(env.and_then(|env| env.get("FAKE_AGENT_API_KEY").cloned()).as_deref(), Some(FAKE_AGENT_KEY));
        let list = workbench.read_with(cx, |w, _| w.agents_list()).expect("the tool window has its view");
        let (_, composer) = workbench.read_with(cx, |w, _| w.docked_agent_session()).expect("the conversation docks");
        let mut shown = agent_conversation(&workbench, cx);
        shown.extend(list.read_with(cx, |list, cx| list.rows(cx)));
        shown.extend(composer.read_with(cx, |composer, cx| composer.buttons(cx)));
        shown.extend(composer.read_with(cx, |composer, cx| composer.chips(cx)));
        shown.push(store.read_with(cx, |store, _| format!("{store:?}")));
        assert!(shown.iter().all(|text| !text.contains(FAKE_AGENT_KEY)), "{shown:?}");

        // ⌘F with the keyboard in the conversation does not open the editor's Find bar.
        cx.update(|window, cx| window.focus(&composer.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(context_is_on(cx, "agentSessionFocus"));
        cx.simulate_keystrokes("cmd-f");
        assert!(workbench.read_with(cx, |w, _| w.find_bar().is_none()));

        // Copy Reply and Quote Reply.
        run_action(cx, "agents.copyReplyMarkdown", Some(serde_json::json!({ "text": "Hello from the fake agent" })));
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some("Hello from the fake agent"));
        run_action(cx, "agents.quoteReply", Some(serde_json::json!({ "task": task, "text": "Hello" })));
        let draft = store.read_with(cx, |store, _| store.composer(&task));
        assert!(draft.text.contains("> Hello"), "{draft:?}");

        // Send to Agent: a Project view file, then the editor's selected line.
        let readme_ipc = ide_core::ipc_path::to_ipc(&readme);
        run_action(cx, "agents.sendToAgent", Some(serde_json::json!([readme_ipc])));
        let draft = store.read_with(cx, |store, _| store.composer(&task));
        assert!(draft.text.contains("@README.md"), "{draft:?}");
        editor.update(cx, |editor, cx| editor.select(0, 8, cx));
        run_action(cx, "agents.sendToAgent", None);
        let draft = store.read_with(cx, |store, _| store.composer(&task));
        assert!(draft.text.contains("@README.md:1"), "{draft:?}");

        // Rename… asks for the name.
        run_action(cx, "agents.renameTask", Some(serde_json::json!(task)));
        let dialog = workbench.read_with(cx, |w, _| w.vcs_dialog_view::<PromptDialog>()).expect("the prompt shows");
        let field = dialog.read_with(cx, |dialog, _| dialog.field().cloned()).expect("the prompt has a field");
        assert_eq!(field.read_with(cx, |field, _| field.text().to_owned()), "Say hello");
        field.update(cx, |field, cx| field.select_all(cx));
        cx.simulate_input("Greeting");
        click_on(cx, "prompt-button-ok");
        wait_long(cx, "the rename", |cx| {
            store.read_with(cx, |store, _| store.task(&task).is_some_and(|t| t.title == "Greeting"))
        });
        run_action(cx, "agents.jumpToLatest", None);

        // Wherever conversations are asked to show, this window docks them in the tool window. Each step
        // changes the placement (it starts in the editor), or opens the task where it is.
        assert_eq!(store.read_with(cx, |store, _| store.session_location), SessionLocation::Editor);
        let id = Some(serde_json::json!(task));
        for (action, arg) in [
            ("agents.useToolWindow", None),
            ("agents.useEditor", None),
            ("agents.openTask", id.clone()),
            ("agents.dockTask", id.clone()),
            ("agents.undockTask", None),
            ("agents.openTaskInEditor", id.clone()),
        ] {
            workbench.update_in(cx, |w, window, cx| w.with_layout(window, cx, |l| l.hide(TOOL_WINDOW)));
            run_action(cx, action, arg);
            wait_long(cx, action, |cx| workbench.read_with(cx, |w, _| w.is_tool_window_visible(TOOL_WINDOW)));
            let docked = workbench.read_with(cx, |w, _| w.docked_agent_session().map(|(s, _)| s));
            let docked = docked.unwrap_or_else(|| panic!("{action} leaves the conversation docked"));
            assert_eq!(docked.read_with(cx, |session, _| session.task().to_owned()), task, "{action}");
        }
        workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
    }

    /// A reply's tool call is listed in the conversation. A turn that ends while the Agents tool window
    /// is hidden raises a notification whose Open shows the task; Retract is not offered to an agent that
    /// cannot roll back; Unarchive brings an
    /// archived task back; Fork is not offered to an agent that cannot copy a session; the task menu
    /// opens; the agents go with the project's trust and come back with it; a reopened project has its
    /// agents again.
    #[gpui_kit::test]
    async fn the_agents_follow_notifications_trust_and_task_commands_and_come_back_with_the_project(
        cx: &mut TestAppContext,
    ) {
        use crate::agents::model::TOOL_WINDOW;
        use crate::agents::store::OPEN_TASK_COMMAND;
        use crate::agents::task_list_view::TaskListEvent;
        use crate::vcs::prompt_dialogs::PromptDialog;
        use ide_agent::AgentStatus;
        let home = start(cx);
        let (folder, workbench, store, cx) = agents_project(cx, home.path());
        let root = folder.path().canonicalize().expect("the folder exists");
        let task = start_agent_task(&workbench, &store, "Run the tool", cx);
        wait_long(cx, "the tool call", |cx| {
            !agent_working(&store, &task, cx)
                && agent_conversation(&workbench, cx).iter().any(|line| line.contains("The tests pass"))
        });
        let lines = agent_conversation(&workbench, cx);
        assert!(lines.iter().any(|line| line.contains("Run cargo test")), "{lines:?}");

        // A turn that ends unseen notifies; its Open shows the task.
        workbench.update_in(cx, |workbench, window, cx| workbench.with_layout(window, cx, |l| l.hide(TOOL_WINDOW)));
        let (_, composer) = workbench.read_with(cx, |w, _| w.docked_agent_session()).expect("the conversation docks");
        store.update(cx, |store, cx| {
            store.update_composer(&task, |draft| draft.text = "Say hello".to_owned());
            cx.notify();
        });
        composer.update(cx, |composer, cx| composer.send(false, cx));
        let open = format!("{OPEN_TASK_COMMAND}:{task}");
        let notice = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| {
                w.notifications().items().iter().flat_map(|n| n.actions.iter()).find(|a| a.command == open).cloned()
            })
        };
        wait_long(cx, "the notification", |cx| notice(cx).is_some());
        let action = EntryAction::Own(notice(cx).expect("the notification has Open"));
        workbench.update_in(cx, |w, window, cx| w.run_notification_action(action, window, cx));
        wait_long(cx, "the task to show", |cx| workbench.read_with(cx, |w, _| w.is_tool_window_visible(TOOL_WINDOW)));
        let docked = workbench.read_with(cx, |w, _| w.docked_agent_session().map(|(s, _)| s)).expect("docked");
        assert_eq!(docked.read_with(cx, |session, _| session.task().to_owned()), task);

        // Retract needs an agent that can roll its session back; the fake one cannot, so nothing is asked.
        assert!(!store.read_with(cx, |store, _| store.task(&task).is_some_and(|t| t.capabilities.rollback)));
        run_action(cx, "agents.retractMessage", Some(serde_json::json!({ "task": task, "turn": 0 })));
        assert!(workbench.read_with(cx, |w, _| w.vcs_dialog_view::<PromptDialog>()).is_none());
        let lines = agent_conversation(&workbench, cx);
        assert!(lines.iter().any(|line| line.starts_with("you: Say hello")), "{lines:?}");

        // Fork needs an agent that can copy its session; the fake one cannot.
        run_action(cx, "agents.forkTask", Some(serde_json::json!(task)));
        assert_eq!(store.read_with(cx, |store, _| store.tasks().len()), 1);

        // The task menu opens over the window.
        let list = workbench.read_with(cx, |w, _| w.agents_list()).expect("the tool window has its view");
        let items = vec![crate::vcs_ops::menu::MenuItem::item(
            "Open",
            crate::agents::task_actions::TaskCommand::Open(task.clone()),
        )];
        list.update(cx, |_, cx| cx.emit(TaskListEvent::ShowMenu { items, position: Default::default() }));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.popup.is_some()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // Unarchive brings the archived task back.
        run_action(cx, "agents.archiveTasks", Some(serde_json::json!([task])));
        wait_long(cx, "the archive", |cx| agent_status(&store, &task, cx) == Some(AgentStatus::Archived));
        run_action(cx, "agents.unarchiveTasks", Some(serde_json::json!([task])));
        wait_long(cx, "the unarchive", |cx| {
            agent_status(&store, &task, cx).is_some_and(|s| s != AgentStatus::Archived)
        });

        // Untrusted, the project lists no agents; trusted again, it does.
        let installed = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| store.provider("fake").is_some_and(|provider| provider.installed))
        };
        let trust = |trusted: bool, cx: &mut VisualTestContext| {
            let root = root.clone();
            let setting = cx.update(|_, cx| {
                cx.global::<Backend>().with_host(move |host| async move {
                    host.set_project_trust(&root, trusted, false).await.map_err(|error| error.message)
                })
            });
            let set = futures::executor::block_on(setting);
            set.expect("the backend runs").expect("the trust is set");
        };
        trust(false, cx);
        wait_long(cx, "the agents to go", |cx| !installed(cx));
        trust(true, cx);
        wait_long(cx, "the agents to come back", |cx| installed(cx));

        // Closed and opened again in the same window, the project has its agents and its task.
        workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.agents_store().is_none()));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        wait_long(cx, "the project", |cx| workbench.read_with(cx, |w, _| w.agents_store().is_some()));
        let store = workbench.read_with(cx, |w, _| w.agents_store()).expect("the project has its agents");
        wait_long(cx, "the task", |cx| store.read_with(cx, |store, _| store.loaded && store.task(&task).is_some()));
        run_action(cx, "agents.showToolWindow", None);
        let list = workbench.read_with(cx, |w, _| w.agents_list()).expect("the tool window has its view");
        let shown = workbench.read_with(cx, |w, _| w.tool_window_view(TOOL_WINDOW)).map(|view| view.entity_id());
        assert_eq!(shown, Some(list.entity_id()), "the tool window shows the reopened project's list");
        let rows = list.read_with(cx, |list, cx| list.rows(cx));
        assert!(rows.iter().any(|row| row.contains("Run the tool")), "{rows:?}");
        workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
    }

    /// A permission request shows in the conversation with its answers while the task waits for the
    /// user; the answer clicked reaches the agent, which carries on with it.
    #[gpui_kit::test]
    async fn a_permission_request_shows_its_answers_and_the_one_clicked_reaches_the_agent(cx: &mut TestAppContext) {
        use ide_agent::AgentStatus;
        let home = start(cx);
        let (_folder, workbench, store, cx) = agents_project(cx, home.path());
        let task = start_agent_task(&workbench, &store, "Run the tests, permission please", cx);
        wait_long(cx, "the permission request", |cx| {
            agent_conversation(&workbench, cx).iter().any(|line| line.starts_with("permission: "))
        });
        let lines = agent_conversation(&workbench, cx);
        let request = lines.iter().find(|line| line.starts_with("permission: ")).expect("the request is listed");
        assert!(request.contains("cargo test"), "{lines:?}");
        wait_long(cx, "the task to wait for the user", |cx| {
            agent_status(&store, &task, cx) == Some(AgentStatus::NeedsInput)
        });

        // The backend numbers permission requests from 1; Allow is the request's first answer.
        click_on(cx, "session-permission-1-0");
        wait_long(cx, "the agent to carry on", |cx| {
            agent_conversation(&workbench, cx).iter().any(|line| line.starts_with("agent: You chose yes"))
        });
        wait_long(cx, "the turn to end", |cx| !agent_working(&store, &task, cx));
        assert_ne!(agent_status(&store, &task, cx), Some(AgentStatus::NeedsInput));
        workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
    }

    /// A reply that arrives while the Agents tool window is hidden leaves its task unread, with the
    /// unread mark in the list and the tool window's badge; Archive moves the task to the archived
    /// group, and Delete, once confirmed, removes it and its conversation.
    #[gpui_kit::test]
    async fn an_unseen_reply_marks_its_task_unread_and_archive_and_delete_remove_it(cx: &mut TestAppContext) {
        use crate::agents::model::TOOL_WINDOW;
        use ide_agent::AgentStatus;
        let home = start(cx);
        let (_folder, workbench, store, cx) = agents_project(cx, home.path());
        let task = start_agent_task(&workbench, &store, "Say hello", cx);
        wait_long(cx, "the first reply", |cx| {
            !agent_working(&store, &task, cx)
                && agent_conversation(&workbench, cx).iter().any(|line| line.starts_with("agent: Hello"))
        });
        assert!(!store.read_with(cx, |store, _| store.is_unread(&task)));

        workbench.update_in(cx, |workbench, window, cx| workbench.with_layout(window, cx, |l| l.hide(TOOL_WINDOW)));
        let (_, composer) = workbench.read_with(cx, |w, _| w.docked_agent_session()).expect("the conversation docks");
        store.update(cx, |store, cx| {
            store.update_composer(&task, |draft| draft.text = "Say hello again".to_owned());
            cx.notify();
        });
        composer.update(cx, |composer, cx| composer.send(false, cx));
        wait_long(cx, "the unseen reply", |cx| {
            !agent_working(&store, &task, cx) && store.read_with(cx, |store, _| store.is_unread(&task))
        });
        let list = workbench.read_with(cx, |w, _| w.agents_list()).expect("the tool window has its view");
        let rows = list.read_with(cx, |list, cx| list.rows(cx));
        assert!(
            rows.iter().any(|row| row.starts_with("Ready | Say hello | Fake Agent | ") && row.ends_with(" | unread")),
            "{rows:?}"
        );
        assert_eq!(store.read_with(cx, |store, _| store.badge()), Some(1));

        run_action(cx, "agents.archiveTasks", Some(serde_json::json!([task])));
        wait_long(cx, "the task to be archived", |cx| agent_status(&store, &task, cx) == Some(AgentStatus::Archived));
        assert!(!store.read_with(cx, |store, _| store.is_unread(&task)));
        let rows = list.read_with(cx, |list, cx| list.rows(cx));
        assert!(rows.iter().any(|row| row.contains("Archived (1)")), "{rows:?}");

        run_action(cx, "agents.deleteTasks", Some(serde_json::json!([task])));
        click_on(cx, "prompt-button-ok");
        wait_long(cx, "the task to be deleted", |cx| store.read_with(cx, |store, _| store.tasks().is_empty()));
        assert_eq!(list.read_with(cx, |list, cx| list.empty_text(cx)), Some("No agent tasks yet".to_owned()));
        assert!(workbench.read_with(cx, |w, _| w.docked_agent_session().is_none()));
        workbench.update_in(cx, |workbench, window, cx| workbench.close_project(window, cx));
    }

    /// The names of the configurations saved in the project's file.
    fn saved_names(root: &std::path::Path) -> Vec<String> {
        let saved = ide_run::load_user_configs(root).expect("the saved configurations are read");
        saved.into_iter().map(|config| config.name).collect()
    }

    fn chooser_lines(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<Vec<String>> {
        let chooser = workbench.read_with(cx, |w, _| w.config_chooser())?;
        Some(chooser.read_with(cx, |chooser, _| chooser.lines()))
    }

    /// Selects the chooser's row `name` (the chooser is open).
    fn select_in_chooser(workbench: &Entity<Workbench>, name: &str, cx: &mut VisualTestContext) {
        let chooser = workbench.read_with(cx, |w, _| w.config_chooser()).expect("the chooser is open");
        let lines = chooser.read_with(cx, |chooser, _| chooser.lines());
        let index = lines.iter().position(|line| line == name).unwrap_or_else(|| panic!("{name} is in {lines:?}"));
        chooser.update(cx, |chooser, cx| chooser.select(index, cx));
    }

    /// The title bar's run widget names the selected configuration and its name opens the chooser, which
    /// lists the project's configurations: choosing one selects it, Enter in the Run mode of Select
    /// Run/Debug Configuration runs it into the Run tool window, Escape closes it, and the action closes an
    /// open one. Run in the widget runs the selection and the widget stops showing it starting.
    #[gpui_kit::test]
    async fn the_run_widget_shows_the_selection_and_the_chooser_selects_runs_and_closes(cx: &mut TestAppContext) {
        let configs = vec![program("hello", "/bin/echo", "hello"), program("bye", "/bin/echo", "bye")];
        let (workbench, cx, _folders, _, store) = run_project(cx, configs);
        let widget = workbench.read_with(cx, |w, _| w.run_widget().cloned()).expect("the title bar has the widget");
        let label = |cx: &mut VisualTestContext| widget.read_with(cx, |widget, cx| widget.label(cx));
        assert_eq!(store.read_with(cx, |store, _| store.selected_id.clone()).as_deref(), Some("hello"));
        assert_eq!(label(cx), "hello");
        let selected_kind = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let context = crate::platform::context::window_context(window, cx);
                let kind = context.get("run.selectedKind");
                matches!(kind, Some(crate::platform::context::ContextValue::Text(kind)) if &**kind == "program")
            })
        };
        assert!(selected_kind(cx), "the selection's kind is published");
        // The debugger is registered: Debug is offered beside Run, for the selection.
        let debug =
            widget.read_with(cx, |widget, cx| widget.buttons(cx)).into_iter().find(|b| b.id == "run-widget-debug");
        assert!(debug.as_ref().is_some_and(|button| button.enabled && button.title == "Debug 'hello'"), "{debug:?}");

        // The name opens the chooser to select: choosing a row selects it and runs nothing.
        click_on(cx, "run-widget-chooser");
        let lines = chooser_lines(&workbench, cx).expect("the chooser is open");
        assert!(lines.contains(&"hello".to_owned()) && lines.contains(&"bye".to_owned()), "{lines:?}");
        let chooser = workbench.read_with(cx, |w, _| w.config_chooser()).expect("the chooser is open");
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.mode()), crate::run::store::ChooserMode::Select);
        assert!(widget.read_with(cx, |widget, cx| widget.chooser_open(cx)));
        select_in_chooser(&workbench, "bye", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(chooser_lines(&workbench, cx).is_none(), "choosing closes the chooser");
        assert_eq!(label(cx), "bye");
        assert!(!widget.read_with(cx, |widget, cx| widget.chooser_open(cx)));
        assert!(store.read_with(cx, |store, _| store.sessions.is_empty()), "selecting runs nothing");
        let stored = cx.update(|_, cx| {
            let root = store.read(cx).project_root.clone().unwrap_or_default();
            let key = format!("{}{root}", crate::run::store::SELECTED_KEY);
            cx.global::<UiStorage>().get(&key).map(str::to_owned)
        });
        assert_eq!(stored.as_deref(), Some("bye"), "the selection is stored");

        // Select Run/Debug Configuration runs: Enter on the selected row runs it.
        run_action(cx, "run.chooseConfig", None);
        let chooser = workbench.read_with(cx, |w, _| w.config_chooser()).expect("the chooser is open");
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.mode()), crate::run::store::ChooserMode::Run);
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.selected()).as_deref(), Some("bye"));
        cx.simulate_keystrokes("enter");
        let session = |cx: &mut VisualTestContext, name: &str| {
            store.read_with(cx, |store, _| store.sessions.iter().find(|s| s.config.name == name).cloned())
        };
        wait_long(cx, "the exit of echo", |cx| session(cx, "bye").is_some_and(|s| !s.is_live()));
        assert_eq!(session(cx, "bye").and_then(|s| s.exit_code), Some(0));
        let view = workbench
            .read_with(cx, |w, _| w.tool_window_view(RUN_TOOL_WINDOW))
            .and_then(|view| view.downcast::<RunView>().ok())
            .expect("the Run tool window holds its view");
        wait_long(cx, "the output", |cx| view.read_with(cx, |view, cx| view.lines(cx)).iter().any(|l| l == "bye"));
        assert!(chooser_lines(&workbench, cx).is_none());

        // Escape closes the chooser; the action closes an open one.
        run_action(cx, "run.chooseConfig", None);
        assert!(chooser_lines(&workbench, cx).is_some());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(chooser_lines(&workbench, cx).is_none(), "Escape closes the chooser");
        assert!(!store.read_with(cx, |store, _| store.chooser_open));
        run_action(cx, "run.chooseConfig", None);
        run_action(cx, "run.chooseConfig", None);
        assert!(chooser_lines(&workbench, cx).is_none(), "the action closes an open chooser");

        // More Actions opens its menu; Escape closes it.
        click_on(cx, "run-widget-more");
        let more = workbench.read_with(cx, |w, _| w.open_menu::<std::convert::Infallible>());
        assert!(more.is_some(), "More Actions shows its menu");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.open_menu::<std::convert::Infallible>()).is_none());

        // Run with nothing selected opens the chooser to run one.
        let driver = workbench.read_with(cx, |w, _| w.run_driver().cloned()).expect("the project runs things");
        driver.update(cx, |driver, cx| driver.select_config(None, cx));
        assert_eq!(label(cx), "Select Configuration");
        run_action(cx, "run.run", None);
        let chooser = workbench.read_with(cx, |w, _| w.config_chooser()).expect("Run opens the chooser");
        assert_eq!(chooser.read_with(cx, |chooser, _| chooser.mode()), crate::run::store::ChooserMode::Run);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        driver.update(cx, |driver, cx| driver.select_config(Some("bye"), cx));

        // Run in the widget runs the selection; once it started, the widget no longer shows it starting.
        let first = session(cx, "bye").expect("a tab").started_at;
        click_on(cx, "run-widget-run");
        wait_long(cx, "the second run of echo", |cx| {
            session(cx, "bye").is_some_and(|s| s.started_at > first && !s.is_live())
                && !widget.read_with(cx, |widget, cx| widget.loading(cx))
        });
    }

    /// Edit Configurations adds a configuration: its type from Add New Configuration, its executable from
    /// the file dialog, its name and arguments typed into the form; OK saves it to the project, selects it
    /// and closes the dialog. Cancel after an edit saves nothing. Create Run Configuration saves the
    /// configuration it is given and opens it in the dialog.
    /// Every built-in type Edit Configurations offers is added through the dialog (Add New
    /// Configuration, its label, Enter), named after its id, its required fields typed, and saved
    /// with OK: the stored configuration has the type's kind and the command line the type builds
    /// from what was typed, and the Run/Debug chooser lists it.
    #[gpui_kit::test]
    async fn edit_configurations_adds_and_saves_a_configuration_of_every_built_in_type(cx: &mut TestAppContext) {
        use crate::run::types::builtin_types;
        const TYPES: [&str; 13] = [
            "shell",
            "program",
            "npm",
            "gradle",
            "maven",
            "godot",
            "pytest",
            "django-test",
            "go",
            "go-test",
            "application",
            "spring-boot",
            "compound",
        ];
        let (workbench, cx, _folders, root, store) = run_project(cx, Vec::new());
        let types = builtin_types();
        let ids: Vec<&str> = types.iter().map(|ty| ty.id.as_str()).collect();
        assert_eq!(ids, TYPES, "the dialog offers these built-in types");
        let dialog = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.edit_configs_dialog().cloned());
        let type_into = |cx: &mut VisualTestContext, key: &str, text: &str| {
            cx.run_until_parked();
            let open = dialog(cx).expect("the dialog is open");
            let field = open.read_with(cx, |dialog, _| dialog.field(key)).unwrap_or_else(|| panic!("{key} is drawn"));
            cx.update(|window, cx| window.focus(&field.focus_handle(cx), cx));
            cx.simulate_keystrokes("cmd-a backspace");
            cx.simulate_input(text);
            cx.run_until_parked();
        };
        for ty in &types {
            let id = ty.id.as_str();
            run_action(cx, "run.editConfigs", None);
            wait_long(cx, "opening Edit Configurations", |cx| dialog(cx).is_some());
            let open = dialog(cx).expect("Edit Configurations opens");
            click_on(cx, "run-edit-add");
            cx.simulate_input(&ty.label);
            cx.run_until_parked();
            // Another type's label can hold this one ("Django Test" holds "Go Test"): walk to its row.
            let rows = open.read_with(cx, |dialog, cx| dialog.type_lines(cx));
            let at = rows
                .iter()
                .position(|row| row.trim_start_matches('>').trim() == ty.label)
                .unwrap_or_else(|| panic!("{id}: {rows:?}"));
            for _ in 0..at {
                cx.simulate_keystrokes("down");
            }
            cx.simulate_keystrokes("enter");
            cx.run_until_parked();
            assert_eq!(open.read_with(cx, |dialog, _| dialog.selected()).as_deref(), Some("Unnamed"), "{id}");
            type_into(cx, "name", id);
            let mut params = ty.defaults.clone();
            for field in ty.fields.iter().filter(|field| field.required && field.is_visible(&ty.defaults)) {
                let typed = format!("{}-{id}", field.key);
                type_into(cx, &format!("param:{}", field.key), &typed);
                params.insert(field.key.clone(), typed);
            }
            if id == "compound" {
                // A compound needs a member: the Shell Script configuration saved first.
                click_on(cx, "run-edit-add-member");
                let rows = open.read_with(cx, |dialog, _| dialog.menu_lines());
                assert_eq!(rows.first().map(String::as_str), Some("shell"), "{rows:?}");
                click_on(cx, "run-edit-menu-0");
                cx.simulate_keystrokes("escape");
                cx.run_until_parked();
            }
            click_on(cx, "run-edit-ok");
            for _ in 0..500 {
                if dialog(cx).is_none() {
                    break;
                }
                cx.executor().advance_clock(Duration::from_millis(20));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(10));
            }
            if let Some(open) = dialog(cx) {
                let lines = open.read_with(cx, |dialog, cx| dialog.form_lines(cx));
                panic!("{id} is not saved: {lines:#?}");
            }
            let saved = ide_run::load_user_configs(&root).expect("the configurations are read");
            let config = saved.iter().find(|config| config.name == id).unwrap_or_else(|| panic!("{id} is saved"));
            if id == "go-test" {
                // Go Test is the test flavour of a Go configuration.
                assert_eq!(config.kind, "go");
                assert_eq!(config.extras.params.get("goKind").map(String::as_str), Some("test"));
            } else {
                assert_eq!(config.kind, id);
            }
            // No Gradle wrapper in the project: Gradle runs as `gradle`.
            let env = crate::run::config_types::TypeEnv { windows: cfg!(windows), gradle_local: false };
            match ty.line(&params, &env) {
                Some(line) => assert_eq!((&config.command, &config.args), (&line.command, &line.args), "{id}"),
                None => assert!(config.command.is_empty(), "{id} runs no process of its own"),
            }
            if id == "compound" {
                let shell = saved.iter().find(|config| config.name == "shell").expect("shell is saved");
                assert_eq!(config.extras.compound, std::slice::from_ref(&shell.id));
            }
            let listed = store.read_with(cx, |store, _| store.configs().iter().any(|c| c.name == id));
            assert!(listed, "the chooser lists {id}");
        }
    }

    #[gpui_kit::test]
    async fn edit_configurations_saves_a_configuration_added_in_its_form_and_cancel_saves_nothing(
        cx: &mut TestAppContext,
    ) {
        let (workbench, cx, _folders, root, store) = run_project(cx, vec![program("hello", "/bin/echo", "hello")]);
        let dialog = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.edit_configs_dialog().cloned());
        let type_into = |cx: &mut VisualTestContext, key: &str, text: &str| {
            cx.run_until_parked();
            let open = dialog(cx).expect("the dialog is open");
            let field = open.read_with(cx, |dialog, _| dialog.field(key)).unwrap_or_else(|| panic!("{key} is drawn"));
            cx.update(|window, cx| {
                let focus = field.focus_handle(cx);
                window.focus(&focus, cx);
            });
            cx.simulate_keystrokes("cmd-a backspace");
            cx.simulate_input(text);
            cx.run_until_parked();
        };

        run_action(cx, "run.editConfigs", None);
        // The dialog opens once the project's templates are read.
        wait_long(cx, "opening Edit Configurations", |cx| dialog(cx).is_some());
        let open = dialog(cx).expect("Edit Configurations opens");
        assert!(store.read_with(cx, |store, _| store.editor_open));
        assert!(open.read_with(cx, |dialog, _| dialog.tree_lines()).contains(&"hello".to_owned()));

        // Add New Configuration › Program.
        click_on(cx, "run-edit-add");
        cx.simulate_input("Program");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(open.read_with(cx, |dialog, _| dialog.selected()).as_deref(), Some("Unnamed"));
        type_into(cx, "name", "greet");
        click_on(cx, "run-edit-browse-param:program");
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|options| {
            assert!(options.files && !options.directories && !options.multiple);
            Some(vec![PathBuf::from("/bin/echo")])
        });
        cx.run_until_parked();
        let program_field = open.read_with(cx, |dialog, _| dialog.field("param:program")).expect("the field is drawn");
        assert_eq!(program_field.read_with(cx, |field, _| field.text().to_owned()), "/bin/echo");
        type_into(cx, "param:arguments", "hi");
        click_on(cx, "run-edit-ok");
        wait_long(cx, "saving the configurations", |cx| dialog(cx).is_none());
        assert_eq!(saved_names(&root), ["hello", "greet"]);
        let saved = ide_run::load_user_configs(&root).expect("the configurations are read");
        let greet = saved.iter().find(|config| config.name == "greet").expect("greet is saved");
        assert_eq!((greet.command.as_str(), greet.args.clone()), ("/bin/echo", vec!["hi".to_owned()]));
        let listed: Vec<String> =
            store.read_with(cx, |store, _| store.configs().iter().map(|c| c.name.clone()).collect());
        assert_eq!(listed, ["hello", "greet"], "the configurations are listed again");
        let selected = store.read_with(cx, |store, _| store.selected_config().map(|c| c.name.clone()));
        assert_eq!(selected.as_deref(), Some("greet"));
        assert!(!store.read_with(cx, |store, _| store.editor_open));

        // Cancel after an edit saves nothing.
        run_action(cx, "run.editConfigs", Some(serde_json::Value::from("hello")));
        wait_long(cx, "opening Edit Configurations", |cx| dialog(cx).is_some());
        let open = dialog(cx).expect("Edit Configurations opens");
        assert_eq!(open.read_with(cx, |dialog, _| dialog.selected()).as_deref(), Some("hello"));
        type_into(cx, "name", "changed");
        assert!(open.read_with(cx, |dialog, _| dialog.is_dirty()));
        click_on(cx, "run-edit-cancel");
        assert!(dialog(cx).is_none(), "Cancel closes the dialog");
        assert_eq!(saved_names(&root), ["hello", "greet"]);

        // Create Run Configuration saves what it is given and opens it in the dialog.
        let made = serde_json::to_value(program("made", "/bin/echo", "made")).expect("a configuration is JSON");
        run_action(cx, "run.createConfig", Some(made));
        wait_long(cx, "creating the configuration", |cx| dialog(cx).is_some());
        // A configuration added in the dialog is kept in the workspace's own file, read after the project's.
        assert_eq!(saved_names(&root), ["hello", "made", "greet"]);
        let local = std::fs::read_to_string(root.join(".openidea/run.local.toml")).unwrap_or_default();
        assert!(local.contains("greet") && !local.contains("made"), "{local}");
        let open = dialog(cx).expect("the dialog is open");
        assert_eq!(open.read_with(cx, |dialog, _| dialog.selected()).as_deref(), Some("made"));
        assert_eq!(store.read_with(cx, |store, _| store.selected_id.clone()).as_deref(), Some("made"));
        click_on(cx, "run-edit-cancel");
        assert!(dialog(cx).is_none());
    }

    /// Delete on a configuration of the chooser removes it from the project; the chooser stays open on
    /// the rest.
    #[gpui_kit::test]
    async fn deleting_a_configuration_in_the_chooser_removes_it_from_the_project(cx: &mut TestAppContext) {
        let configs = vec![program("hello", "/bin/echo", "hello"), program("bye", "/bin/echo", "bye")];
        let (workbench, cx, _folders, root, store) = run_project(cx, configs);
        run_action(cx, "run.chooseConfig", None);
        select_in_chooser(&workbench, "hello", cx);
        cx.simulate_keystrokes("delete");
        wait_long(cx, "deleting the configuration", |cx| {
            store.read_with(cx, |store, _| store.configs().iter().map(|c| c.id.clone()).collect::<Vec<_>>()) == ["bye"]
        });
        assert_eq!(saved_names(&root), ["bye"]);
        let lines = chooser_lines(&workbench, cx).expect("the chooser stays open");
        assert!(!lines.contains(&"hello".to_owned()) && lines.contains(&"bye".to_owned()), "{lines:?}");
    }

    #[test]
    fn titles_name_the_project_then_the_file() {
        let file = std::path::Path::new("/work/shop/src/main.rs");
        assert_eq!(window_title(Some("shop"), Some(file), false, None, "OpenIDEA"), "shop – main.rs");
        assert_eq!(window_title(Some("shop"), Some(file), true, None, "OpenIDEA"), "shop – /work/shop/src/main.rs");
        assert_eq!(window_title(Some("shop"), None, false, Some("me@box"), "OpenIDEA"), "shop [me@box]");
        assert_eq!(
            window_title(Some("shop"), Some(file), false, Some("me@box"), "OpenIDEA"),
            "shop [me@box] – main.rs"
        );
        assert_eq!(window_title(None, Some(file), false, None, "OpenIDEA"), "main.rs");
        assert_eq!(window_title(None, None, false, None, "OpenIDEA"), "OpenIDEA");
        assert_eq!(window_title(Some(""), None, false, None, "OpenIDEA"), "OpenIDEA");
    }

    #[gpui_kit::test]
    async fn a_file_that_cannot_open_is_a_message_in_the_window(cx: &mut TestAppContext) {
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        // A folder is not a file to edit.
        workbench.update_in(cx, |workbench, window, cx| workbench.open_file(folder.path().to_path_buf(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some() || w.failure().is_some()));
        workbench.read_with(cx, |w, _| {
            assert!(w.editor().is_none());
            assert!(w.failure().is_some_and(|message| message.contains("is a folder")), "{:?}", w.failure());
        });
    }

    /// Runs git in `dir` without the machine's own configuration; its output, trimmed.
    pub(super) fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
        let run = || {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs")
        };
        let mut output = run();
        // The open window reads the repository too, which holds its index for a moment.
        for _ in 0..100 {
            if output.status.success() || !String::from_utf8_lossy(&output.stderr).contains("index.lock") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            output = run();
        }
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A repository on `main` with an identity of its own and a first commit.
    pub(super) fn init_repo(root: &std::path::Path) {
        git_in(root, &["init", "-q", "-b", "main"]);
        git_in(root, &["config", "user.name", "Ada"]);
        git_in(root, &["config", "user.email", "ada@example.test"]);
        // What the backend's git reads from the machine elsewhere is the repository's own here.
        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.hooksPath", ".git/hooks"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("merge.ff", "true"),
            ("tag.forceSignAnnotated", "false"),
            ("rerere.enabled", "false"),
            ("diff.context", "3"),
            ("core.excludesFile", "/dev/null"),
            ("pull.rebase", "false"),
            ("push.default", "simple"),
            ("status.showUntrackedFiles", "normal"),
            ("diff.noprefix", "false"),
            ("diff.mnemonicPrefix", "false"),
        ] {
            git_in(root, &["config", key, value]);
        }
        // The IDE's per-user files (change lists, shelf) stay out of `git status`.
        std::fs::create_dir_all(root.join(".git/info")).expect("the folder is made");
        std::fs::write(root.join(".git/info/exclude"), "/.openidea/\n").expect("the exclude file is written");
        std::fs::write(root.join("a.txt"), "one\n").expect("the file is written");
        git_in(root, &["add", "."]);
        git_in(root, &["commit", "-q", "-m", "first"]);
    }

    /// The branch the repository at `root` is on.
    pub(super) fn git_head(root: &std::path::Path) -> String {
        git_in(root, &["rev-parse", "--abbrev-ref", "HEAD"])
    }

    /// The local branches of the repository at `root`.
    fn git_branches(root: &std::path::Path) -> Vec<String> {
        git_in(root, &["branch", "--format=%(refname:short)"]).lines().map(str::to_owned).collect()
    }

    /// A trusted project on a repository `prepare` makes, open in a window whose branch widget has
    /// read it.
    pub(super) fn git_project(
        cx: &mut TestAppContext,
        prepare: impl FnOnce(&std::path::Path),
    ) -> (Entity<Workbench>, &mut VisualTestContext, [tempfile::TempDir; 2], PathBuf) {
        let home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        prepare(&root);
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, true, false)).expect("the project is trusted");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        wait_long(cx, "the repository is read", |cx| !branch_label(&workbench, cx).is_empty());
        (workbench, cx, [home, folder], root)
    }

    /// A Git command whose future is dropped before its backend call returns opens the window's gate:
    /// the next operation of the window runs to the end and the repository is read again.
    #[gpui_kit::test]
    async fn a_git_command_dropped_before_it_ends_lets_the_next_operation_run(cx: &mut TestAppContext) {
        let (workbench, cx, _dirs, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["branch", "feature"]);
        });
        let hanging = workbench.update_in(cx, |_, window, cx| {
            cx.spawn_in(window, async move |this, cx| {
                let never = |_, _| futures::future::pending::<ide_core::IdeResult<()>>();
                branches::run_op::<(), _>(&this, cx, "Hang", None, false, never).await
            })
        });
        wait_long(cx, "the hanging command takes the gate", |cx| workbench.read_with(cx, |w, _| w.git_gate_closed()));
        drop(hanging);
        cx.run_until_parked();
        assert!(!workbench.read_with(cx, |w, _| w.git_gate_closed()), "the dropped command opened the gate");

        choose_in_branch_menu(&workbench, "feature", "Checkout", cx);
        wait_long(cx, "the checkout", |cx| branch_label(&workbench, cx) == "feature");
        assert_eq!(git_head(&root), "feature");
        wait_for_notice(&workbench, "Checked out feature", cx);
        assert!(!workbench.read_with(cx, |w, _| w.git_gate_closed()), "the checkout released the gate");
    }

    /// While a Git command of the window runs, neither a change the backend reports nor the reread
    /// after a commit makes the Commit panel read the repository itself (a status read writes the
    /// index the command may need): the window's own read waits for the command, and once it has
    /// ended the panel shows what that read found.
    #[gpui_kit::test]
    async fn the_commit_panel_reads_nothing_itself_while_a_git_command_of_the_window_runs(cx: &mut TestAppContext) {
        let (workbench, cx, _dirs, root) = git_project(cx, init_repo);
        run_action(cx, "vcs.commit", None);
        let (panel, _) = commit_parts(&workbench, cx);
        let own_reads = |cx: &mut VisualTestContext| panel.read_with(cx, |panel, _| panel.own_reads());
        assert_eq!(own_reads(cx), 0, "the panel starts from the window's read");
        let hanging = workbench.update_in(cx, |_, window, cx| {
            cx.spawn_in(window, async move |this, cx| {
                let never = |_, _| futures::future::pending::<ide_core::IdeResult<()>>();
                branches::run_op::<(), _>(&this, cx, "Hang", None, false, never).await
            })
        });
        wait_long(cx, "the hanging command takes the gate", |cx| workbench.read_with(cx, |w, _| w.git_gate_closed()));

        std::fs::write(root.join("a.txt"), "changed\n").expect("the file is written");
        let changed = IdeEvent::VcsChanged { root: ide_core::IpcPath(root.clone()) };
        cx.update(|_, cx| cx.global::<Backend>().emit(Some("w1"), changed));
        workbench.update(cx, |workbench, cx| workbench.reread_commit_views(cx));
        wait_long(cx, "the window's read waits for the command", |cx| {
            workbench.read_with(cx, |w, _| w.vcs_report().contains("read deferred=true"))
        });
        assert_eq!(own_reads(cx), 0, "the panel read nothing while the command ran");

        drop(hanging);
        wait_long(cx, "the panel shows the window's read", |cx| {
            panel.read_with(cx, |panel, _| panel.lines().iter().any(|line| line.contains("a.txt")))
        });
        assert_eq!(own_reads(cx), 0);
    }

    /// A remote operation whose future is dropped while it waits for the gate leaves nothing marked
    /// as running: the next Fetch of the window runs and is told, and no warning says that another
    /// remote operation is in progress.
    #[gpui_kit::test]
    async fn a_remote_operation_dropped_before_it_ends_lets_the_next_one_run(cx: &mut TestAppContext) {
        let remote = tempfile::tempdir().expect("a temporary folder");
        let parent = remote.path().canonicalize().expect("the folder exists");
        let bare_path = parent.join("origin.git").to_string_lossy().into_owned();
        git_in(&parent, &["init", "-q", "--bare", "-b", "main", &bare_path]);
        let (workbench, cx, _dirs, _root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["remote", "add", "origin", &bare_path]);
            git_in(root, &["push", "-q", "-u", "origin", "main"]);
        });
        let hanging = workbench.update_in(cx, |_, window, cx| {
            cx.spawn_in(window, async move |this, cx| {
                let never = |_, _| futures::future::pending::<ide_core::IdeResult<()>>();
                branches::run_op::<(), _>(&this, cx, "Hang", None, false, never).await
            })
        });
        wait_long(cx, "the hanging command takes the gate", |cx| workbench.read_with(cx, |w, _| w.git_gate_closed()));
        let waiting = workbench.update_in(cx, |_, window, cx| {
            cx.spawn_in(window, async move |this, cx| {
                let fetch = ide_host::ops::vcs::RemoteOp::Fetch;
                branches::run_remote(&this, cx, fetch, None, vec![None]).await
            })
        });
        wait_long(cx, "the fetch is marked as running", |cx| {
            workbench.read_with(cx, |w, _| w.vcs_state().is_some_and(|state| state.remote_busy.is_some()))
        });
        drop(waiting);
        drop(hanging);
        cx.run_until_parked();

        run_action(cx, "vcs.fetch", None);
        wait_for_notice(&workbench, "Fetch successful", cx);
        let titles = notification_titles(&workbench, cx);
        assert!(!titles.contains(&"Another remote operation is in progress".to_owned()), "{titles:?}");
        assert!(workbench.read_with(cx, |w, _| w.vcs_state().is_some_and(|state| state.remote_busy.is_none())));
    }

    /// In Restricted Mode no Git hook of the project runs: a checkout from the Branches popup is
    /// refused, HEAD stays and the `post-checkout` hook writes nothing. Once the project is trusted the
    /// same checkout runs, and so does its hook.
    #[gpui_kit::test]
    async fn a_project_in_restricted_mode_runs_no_git_hook_until_it_is_trusted(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        let marker = tempfile::tempdir().expect("a temporary folder");
        let ran = marker.path().join("hook-ran");
        let home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        init_repo(&root);
        git_in(&root, &["branch", "feature"]);
        let hook = root.join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().expect("a folder")).expect("the hooks folder is made");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).expect("the hook is written");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("the hook runs");
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, false, false)).expect("previewed");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        wait_long(cx, "the repository is read", |cx| branch_label(&workbench, cx) == "main");
        assert!(workbench.read_with(cx, |w, cx| w.project_restricted(cx)));

        choose_in_branch_menu(&workbench, "feature", "Checkout", cx);
        let refused = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| {
                w.notifications()
                    .items()
                    .iter()
                    .any(|n| n.body.as_deref().is_some_and(|b| b.contains("Restricted Mode")))
            })
        };
        wait_long(cx, "the refused checkout", refused);
        assert_eq!(git_head(&root), "main");
        assert!(!ran.exists(), "the hook did not run");

        run_action(cx, "project.trust", None);
        wait_long(cx, "the trust", |cx| workbench.read_with(cx, |w, cx| !w.project_restricted(cx)));
        choose_in_branch_menu(&workbench, "feature", "Checkout", cx);
        wait_long(cx, "the checkout", |cx| branch_label(&workbench, cx) == "feature");
        assert_eq!(git_head(&root), "feature");
        assert!(ran.exists(), "the hook ran in the trusted project");
        drop(home);
    }

    /// `a.txt` reads "main\n" on `main` and "feature\n" on `feature`, both from "one\n": merging one
    /// into the other conflicts. `main` is checked out.
    fn conflicting_branches(root: &std::path::Path) {
        init_repo(root);
        git_in(root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "feature\n").expect("the file is written");
        git_in(root, &["commit", "-q", "-am", "feature side"]);
        git_in(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "main\n").expect("the file is written");
        git_in(root, &["commit", "-q", "-am", "main side"]);
    }

    /// Merges `feature` into `main` from the Branches popup; the merge stops on the conflict and the
    /// Conflicts dialog opens.
    fn merge_into_conflicts(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::vcs::conflicts_dialog::ConflictsDialog> {
        choose_in_branch_menu(workbench, "feature", "Merge 'feature' into 'main'", cx);
        wait_long(cx, "the Conflicts dialog", |cx| workbench.read_with(cx, |w, _| w.conflicts_dialog().is_some()));
        workbench.read_with(cx, |w, _| w.conflicts_dialog()).expect("the dialog is open")
    }

    /// The merge editor, once it shows the file's versions.
    fn merge_view_loaded(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::vcs::merge_view::MergeView> {
        wait_long(cx, "the merge editor's versions", |cx| {
            let view = workbench.read_with(cx, |w, _| w.merge_view());
            view.is_some_and(|view| view.read_with(cx, |view, _| view.text().is_some()))
        });
        workbench.read_with(cx, |w, _| w.merge_view()).expect("the merge editor is shown")
    }

    pub(super) fn notice_titled(
        workbench: &Entity<Workbench>,
        title: &str,
        cx: &mut VisualTestContext,
    ) -> Option<notifications::IdeNotification> {
        workbench.read_with(cx, |w, _| w.notifications().items().iter().find(|n| n.title == title).cloned())
    }

    /// What the branch widget of the title bar says.
    pub(super) fn branch_label(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> String {
        let Some(widget) = workbench.read_with(cx, |w, _| w.branch_widget().cloned()) else { return String::new() };
        widget.update(cx, |widget, cx| widget.label(&mut cx.default_global::<crate::platform::i18n::Localization>().0))
    }

    fn branches_rows(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<Vec<String>> {
        let popup = workbench.read_with(cx, |w, _| w.branches_popup())?;
        Some(popup.read_with(cx, |popup, cx| popup.rows(cx)))
    }

    /// Opens the Branches popup on branch `name`'s submenu and clicks its row `label`.
    fn choose_in_branch_menu(workbench: &Entity<Workbench>, name: &str, label: &str, cx: &mut VisualTestContext) {
        run_action(cx, "vcs.branches", None);
        let popup = workbench.read_with(cx, |w, _| w.branches_popup()).expect("the popup is open");
        cx.simulate_input(name);
        cx.run_until_parked();
        // The search keeps the groups: ↓ goes from the group to the branch.
        // A row names the branch first, then its marks (★, counts, upstream).
        let selected = |cx: &mut VisualTestContext| {
            let row = popup.read_with(cx, |popup, cx| popup.selected(cx)).unwrap_or_default();
            row.split_whitespace().next().unwrap_or_default().to_owned()
        };
        for _ in 0..4 {
            if selected(cx) == name {
                break;
            }
            cx.simulate_keystrokes("down");
        }
        assert_eq!(selected(cx), name, "the search finds the branch");
        cx.simulate_keystrokes("right");
        let rows = popup.read_with(cx, |popup, _| popup.submenu_rows());
        let index = rows
            .iter()
            .position(|row| row.trim_start_matches("> ") == label)
            .unwrap_or_else(|| panic!("{label} is in {rows:?}"));
        let selector: &'static str = Box::leak(format!("branches-submenu-{index}").into_boxed_str());
        click_on(cx, selector);
    }

    /// The dialog a Git operation waits on, once it shows.
    pub(super) fn wait_for_prompt(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::vcs::prompt_dialogs::PromptDialog> {
        wait_long(cx, "the dialog", |cx| workbench.read_with(cx, |w, _| w.vcs_prompt().is_some()));
        workbench.read_with(cx, |w, _| w.vcs_prompt()).expect("the dialog is open")
    }

    fn prompt_text(dialog: &Entity<crate::vcs::prompt_dialogs::PromptDialog>, cx: &mut VisualTestContext) -> String {
        let field = dialog.read_with(cx, |dialog, _| dialog.field().cloned()).expect("the dialog has a field");
        field.read_with(cx, |field, _| field.text().to_owned())
    }

    fn prompt_errors(
        dialog: &Entity<crate::vcs::prompt_dialogs::PromptDialog>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        dialog.update(cx, |dialog, cx| dialog.errors(cx))
    }

    /// Replaces the text of the dialog's field by typing.
    pub(super) fn retype(
        dialog: &Entity<crate::vcs::prompt_dialogs::PromptDialog>,
        text: &str,
        cx: &mut VisualTestContext,
    ) {
        for _ in 0..prompt_text(dialog, cx).chars().count() {
            cx.simulate_keystrokes("backspace");
        }
        cx.simulate_input(text);
        cx.run_until_parked();
    }

    pub(super) fn notification_titles(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Vec<String> {
        workbench.read_with(cx, |w, _| w.notifications().items().iter().map(|n| n.title.clone()).collect())
    }

    /// The hint showing at the caret of the active editor, once the effects under way are delivered.
    pub(super) fn editor_hint(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<String> {
        cx.run_until_parked();
        workbench.read_with(cx, |w, cx| {
            let intelligence = crate::editor::intelligence::EditorIntelligence::of(w.editor()?, cx)?;
            intelligence.read(cx).hint().map(str::to_owned)
        })
    }

    /// Waits for the notification `title` of a Git operation. The backend's event on what the operation
    /// changed can refresh the window before the operation's own read returns, and the notice comes only
    /// after that read.
    pub(super) fn wait_for_notice(workbench: &Entity<Workbench>, title: &str, cx: &mut VisualTestContext) {
        wait_long(cx, &format!("the notice {title}"), |cx| {
            notification_titles(workbench, cx).iter().any(|n| n == title)
        });
    }

    /// The branch widget names the current branch; Branches… opens the popup listing the local
    /// branches with the current one marked, and a click on Checkout in another branch's submenu checks
    /// it out, closes the popup and the widget follows. The action closes an open popup, Escape closes
    /// it too, and Commit… shows the Commit tool window.
    #[gpui_kit::test]
    async fn the_branch_widget_names_the_branch_and_the_popup_checks_out_another(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["branch", "feature"]);
        });
        assert_eq!(branch_label(&workbench, cx), "main");
        let widget = workbench.read_with(cx, |w, _| w.branch_widget().cloned()).expect("the title bar has the widget");
        assert!(widget.read_with(cx, |widget, _| widget.is_shown()));

        click_on(cx, "vcs-branch-widget");
        wait_long(cx, "the branches are listed", |cx| {
            branches_rows(&workbench, cx).is_some_and(|rows| rows.iter().any(|row| row.trim() == "feature"))
        });
        let rows = branches_rows(&workbench, cx).expect("the popup is open");
        assert!(rows.iter().any(|row| row.trim() == "main [current]"), "{rows:?}");
        assert!(rows.iter().any(|row| row == "New Branch…"), "{rows:?}");
        assert!(widget.read_with(cx, |widget, _| widget.state().open), "the widget is pressed while the popup is open");

        // A gear option and a star are stored, and the popup stays.
        click_on(cx, "branches-options");
        click_on(cx, "branches-option-0");
        click_on(cx, "branches-star-local:feature");
        assert!(branches_rows(&workbench, cx).is_some(), "the popup stays open");
        let stored = |key: String, cx: &mut VisualTestContext| {
            cx.update(|_, cx| cx.global::<UiStorage>().get(&key).map(str::to_owned))
        };
        let options = stored(super::branches::OPTIONS_KEY.to_owned(), cx).expect("the options are stored");
        let options: serde_json::Value = serde_json::from_str(&options).expect("the options are JSON");
        assert_eq!(options, serde_json::json!({ "groupByPrefix": false, "showRecent": true, "showTags": true }));
        let favorites = crate::vcs_ops::branch_ops::favorites_key(Some(&root.to_string_lossy()));
        assert_eq!(stored(favorites, cx).as_deref(), Some(r#"["main","feature"]"#));
        let rows = branches_rows(&workbench, cx).expect("the popup is open");
        assert!(rows.iter().any(|row| row.trim() == "feature ★"), "{rows:?}");

        // The action closes the open popup; Escape closes it too.
        run_action(cx, "vcs.branches", None);
        assert!(branches_rows(&workbench, cx).is_none());
        assert!(!widget.read_with(cx, |widget, _| widget.state().open));
        run_action(cx, "vcs.branches", None);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(branches_rows(&workbench, cx).is_none(), "Escape closes the popup");

        // Checkout in the submenu of another branch, with the mouse.
        choose_in_branch_menu(&workbench, "feature", "Checkout", cx);
        assert!(branches_rows(&workbench, cx).is_none(), "choosing closes the popup");
        wait_long(cx, "the checkout", |cx| branch_label(&workbench, cx) == "feature");
        assert_eq!(git_head(&root), "feature");
        // The recent branches are stored in the same step as the notice.
        wait_for_notice(&workbench, "Checked out feature", cx);
        let recent = cx.update(|_, cx| {
            let key = crate::vcs_ops::branch_ops::recent_key(Some(&root.to_string_lossy()));
            cx.global::<UiStorage>().get(&key).map(str::to_owned)
        });
        assert_eq!(recent.as_deref(), Some(r#"["feature","main"]"#), "both branches are recent");

        run_action(cx, "vcs.commit", None);
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(COMMIT_TOOL_WINDOW)));

        // A checkout made outside the window reaches the widget through the backend's events.
        git_in(&root, &["checkout", "-q", "main"]);
        wait_long(cx, "the outside checkout", |cx| branch_label(&workbench, cx) == "main");
    }

    /// New Branch… suggests the current branch's name and refuses it and an invalid name as the
    /// dialog's model says; OK on a free name creates and checks out the branch. Rename… in a branch's
    /// submenu renames it; Delete deletes an unmerged branch without asking and says its commits were
    /// discarded, and the notification's Restore brings it back. Checkout Tag or Revision… checks out
    /// what is typed.
    #[gpui_kit::test]
    async fn new_branch_rename_and_delete_change_the_repositorys_branches(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["checkout", "-q", "-b", "topic"]);
            std::fs::write(root.join("t.txt"), "topic\n").expect("the file is written");
            git_in(root, &["add", "."]);
            git_in(root, &["commit", "-q", "-m", "topic work"]);
            git_in(root, &["checkout", "-q", "main"]);
        });
        assert_eq!(branch_label(&workbench, cx), "main");

        // New Branch: the current branch is suggested, and refused.
        run_action(cx, "vcs.newBranch", None);
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Create New Branch");
        assert_eq!(prompt_text(&dialog, cx), "main");
        assert_eq!(prompt_errors(&dialog, cx), ["Branch 'main' already exists"]);
        assert!(dialog.read_with(cx, |dialog, _| dialog.offers_overwrite()));
        retype(&dialog, "bad.", cx);
        assert_eq!(prompt_errors(&dialog, cx), ["Invalid branch name"]);
        cx.simulate_keystrokes("enter");
        assert!(workbench.read_with(cx, |w, _| w.vcs_prompt().is_some()), "an invalid name is not accepted");
        retype(&dialog, "topic", cx);
        assert_eq!(prompt_errors(&dialog, cx), ["Branch 'topic' already exists"]);
        retype(&dialog, "my next", cx);
        assert_eq!(prompt_text(&dialog, cx), "my-next", "a space becomes a separator");
        assert!(prompt_errors(&dialog, cx).is_empty());
        cx.simulate_keystrokes("enter");
        assert!(workbench.read_with(cx, |w, _| w.vcs_prompt().is_none()), "OK closes the dialog");
        wait_long(cx, "the new branch", |cx| branch_label(&workbench, cx) == "my-next");
        assert_eq!(git_head(&root), "my-next");
        wait_for_notice(&workbench, "Created and checked out my-next", cx);

        // Rename in the branch's submenu.
        choose_in_branch_menu(&workbench, "topic", "Rename…", cx);
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Rename Branch topic");
        retype(&dialog, "topical", cx);
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the rename", |_| git_branches(&root).contains(&"topical".to_owned()));
        assert!(!git_branches(&root).contains(&"topic".to_owned()));

        // Delete an unmerged branch: nothing is asked, the notification says what was discarded.
        wait_long(cx, "the renamed branch is listed", |cx| {
            workbench.read_with(cx, |w, _| {
                w.vcs_state().is_some_and(|state| state.branches.iter().any(|branch| branch.name == "topical"))
            })
        });
        choose_in_branch_menu(&workbench, "topical", "Delete", cx);
        assert!(workbench.read_with(cx, |w, _| w.vcs_prompt().is_none()), "deleting asks nothing");
        wait_long(cx, "the deletion", |cx| {
            workbench
                .read_with(cx, |w, _| w.notifications().items().iter().any(|n| n.title == "Deleted branch topical"))
        });
        assert!(!git_branches(&root).contains(&"topical".to_owned()));
        let notice = workbench.read_with(cx, |w, _| {
            w.notifications().items().iter().find(|n| n.title == "Deleted branch topical").cloned()
        });
        let notice = notice.expect("the deletion is told");
        assert_eq!(notice.body.as_deref(), Some("Unmerged commits were discarded"));
        let labels: Vec<&str> = notice.actions.iter().map(|action| action.label.as_str()).collect();
        assert_eq!(labels, ["Restore", "View Commits"]);
        let restore = EntryAction::Own(notice.actions[0].clone());
        workbench.update_in(cx, |w, window, cx| w.run_notification_action(restore, window, cx));
        wait_long(cx, "the restore", |_| git_branches(&root).contains(&"topical".to_owned()));

        // Checkout Tag or Revision.
        run_action(cx, "vcs.checkoutRevision", None);
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Checkout Tag or Revision");
        cx.simulate_input("topical");
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the checkout", |cx| branch_label(&workbench, cx) == "topical");
        assert_eq!(git_head(&root), "topical");
    }

    /// Push… lists the commit made since the last push and sends it to the local bare repository
    /// `origin`; Fetch brings in a commit another clone pushed, and the widget shows it incoming.
    #[gpui_kit::test]
    async fn push_sends_a_new_commit_to_origin_and_fetch_shows_what_came_in(cx: &mut TestAppContext) {
        let remote = tempfile::tempdir().expect("a temporary folder");
        let bare = remote.path().canonicalize().expect("the folder exists").join("origin.git");
        let bare_path = bare.to_string_lossy().into_owned();
        let parent = remote.path().canonicalize().expect("the folder exists");
        git_in(&parent, &["init", "-q", "--bare", "-b", "main", &bare_path]);
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["remote", "add", "origin", &bare_path]);
            git_in(root, &["push", "-q", "-u", "origin", "main"]);
        });
        std::fs::write(root.join("b.txt"), "two\n").expect("the file is written");
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "second"]);

        run_action(cx, "vcs.push", None);
        wait_long(cx, "the Push dialog", |cx| workbench.read_with(cx, |w, _| w.push_dialog().is_some()));
        let dialog = workbench.read_with(cx, |w, _| w.push_dialog()).expect("the Push dialog is open");
        let commits = dialog.read_with(cx, |dialog, _| dialog.commits(0));
        assert_eq!(commits.len(), 1, "{commits:?}");
        assert!(commits[0].ends_with(" second"), "{commits:?}");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.checked()), [root.to_string_lossy().into_owned()]);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.push_dialog().is_none()), "Push closes the dialog");
        wait_long(cx, "the push", |_| git_in(&bare, &["log", "-1", "--format=%s", "main"]) == "second");
        wait_long(cx, "the push is told", |cx| {
            notification_titles(&workbench, cx).contains(&"Pushed 1 commit to origin/main".to_owned())
        });
        let widget = workbench.read_with(cx, |w, _| w.branch_widget().cloned()).expect("the title bar has the widget");
        assert_eq!(widget.read_with(cx, |widget, _| widget.counts()), (0, 0));

        // Another clone pushes a commit; Fetch shows it incoming.
        let other = parent.join("other");
        git_in(&parent, &["clone", "-q", &bare_path, &other.to_string_lossy()]);
        git_in(&other, &["config", "user.name", "Bob"]);
        git_in(&other, &["config", "user.email", "bob@example.test"]);
        std::fs::write(other.join("c.txt"), "three\n").expect("the file is written");
        git_in(&other, &["add", "."]);
        git_in(&other, &["commit", "-q", "-m", "third"]);
        git_in(&other, &["push", "-q", "origin", "main"]);
        run_action(cx, "vcs.fetch", None);
        wait_long(cx, "the fetch", |cx| widget.read_with(cx, |widget, _| widget.counts()) == (1, 0));
        wait_for_notice(&workbench, "Fetch successful", cx);
    }

    /// Merge in a branch's submenu stops on conflicts: the Conflicts dialog opens on the file and the
    /// conflicts are told with Resolve…; the widget says the branch is merging and the title bar shows
    /// Resolve and Abort Merge, whose click (once the dialog is closed) aborts the merge. Checkout over
    /// local changes in the way asks about Smart Checkout, and Escape leaves the branch as it is.
    #[gpui_kit::test]
    async fn a_merge_stopped_on_conflicts_is_aborted_from_the_title_bar(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            git_in(root, &["checkout", "-q", "-b", "feature"]);
            std::fs::write(root.join("a.txt"), "feature\n").expect("the file is written");
            git_in(root, &["commit", "-q", "-am", "feature side"]);
            git_in(root, &["checkout", "-q", "main"]);
            std::fs::write(root.join("a.txt"), "main\n").expect("the file is written");
            git_in(root, &["commit", "-q", "-am", "main side"]);
        });
        assert!(workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).is_none()), "nothing is stopped");

        choose_in_branch_menu(&workbench, "feature", "Merge 'feature' into 'main'", cx);
        wait_long(cx, "the merge stops", |cx| workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).is_some()));
        wait_long(cx, "the Conflicts dialog", |cx| workbench.read_with(cx, |w, _| w.conflicts_dialog().is_some()));
        let dialog = workbench.read_with(cx, |w, _| w.conflicts_dialog()).expect("the dialog is open");
        let names: Vec<String> =
            dialog.read_with(cx, |dialog, _| dialog.rows().into_iter().map(|row| row.name).collect());
        assert_eq!(names, ["a.txt"]);
        assert!(
            !notification_titles(&workbench, cx).contains(&"Merge feature into main failed".to_owned()),
            "a stop on conflicts is not a failure"
        );
        let told = workbench
            .read_with(cx, |w, _| w.notifications().items().iter().find(|n| n.title == "Merge conflicts").cloned());
        let told = told.expect("the conflicts are told");
        let actions: Vec<&str> = told.actions.iter().map(|action| action.label.as_str()).collect();
        assert_eq!(actions, ["Resolve…"]);
        assert_eq!(branch_label(&workbench, cx), "Merging main");
        let operation = cx.update(|window, cx| {
            let context = crate::platform::context::window_context(window, cx);
            context.get("vcsOperation").cloned()
        });
        assert!(
            matches!(operation, Some(crate::platform::context::ContextValue::Text(op)) if &*op == "merge"),
            "the operation is published"
        );
        let merge = workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).cloned()).expect("the buttons show");
        assert_eq!(merge.read_with(cx, |merge, cx| merge.labels(cx)), ["Resolve Conflicts…", "Abort Merge"]);

        // The dialog covers the window: it is closed first.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.conflicts_dialog().is_none()), "Escape closes the dialog");
        click_on(cx, "merge-rebase-1");
        wait_long(cx, "the abort", |cx| workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).is_none()));
        assert!(!root.join(".git/MERGE_HEAD").exists(), "the merge is aborted");
        wait_for_notice(&workbench, "Merge aborted", cx);
        assert_eq!(branch_label(&workbench, cx), "main");

        // Local changes in the way of a checkout: Smart Checkout is asked about.
        std::fs::write(root.join("a.txt"), "local\n").expect("the file is written");
        choose_in_branch_menu(&workbench, "feature", "Checkout", cx);
        let dialog = wait_for_prompt(&workbench, cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Git Checkout Problem");
        let buttons: Vec<String> =
            dialog.read_with(cx, |dialog, _| dialog.buttons().iter().map(|button| button.label.clone()).collect());
        assert_eq!(buttons, ["Smart Checkout", "Force Checkout", "Don't Checkout"]);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.vcs_prompt().is_none()), "Escape closes the question");
        assert_eq!(git_head(&root), "main");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("the file is read"), "local\n");
    }

    /// A merge that stops on conflicts opens the Conflicts dialog on the conflicted file; Accept Yours
    /// writes our side, stages it, and the dialog, with nothing left, offers Commit…, which shows the
    /// Commit tool window.
    #[gpui_kit::test]
    async fn a_merge_stopped_on_conflicts_opens_the_conflicts_dialog_and_accept_yours_resolves_the_file(
        cx: &mut TestAppContext,
    ) {
        use crate::vcs::conflicts_dialog::{ACCEPT_YOURS, CONCLUDE, Conclusion};
        let (workbench, cx, _folders, root) = git_project(cx, conflicting_branches);
        let dialog = merge_into_conflicts(&workbench, cx);
        let rows = dialog.read_with(cx, |dialog, _| dialog.rows());
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["a.txt"]);
        assert!(rows[0].selected, "the first file is selected");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.conclusion()), None);
        assert!(
            !notification_titles(&workbench, cx).contains(&"Merge feature into main failed".to_owned()),
            "a stop on conflicts is not a failure"
        );

        dialog.update(cx, |dialog, cx| dialog.press(ACCEPT_YOURS, cx));
        wait_long(cx, "the file is resolved", |cx| dialog.read_with(cx, |dialog, _| dialog.rows().is_empty()));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("the file is read"), "main\n");
        assert_eq!(git_in(&root, &["ls-files", "-u"]), "", "nothing is left unmerged");
        assert!(root.join(".git/MERGE_HEAD").exists(), "the merge is still to be committed");
        wait_for_notice(&workbench, "a.txt resolved", cx);
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.conclusion()), Some(Conclusion::Commit));
        let buttons = dialog.read_with(cx, |dialog, _| dialog.buttons());
        assert_eq!((buttons[0].id, buttons[0].label.as_str(), buttons[0].primary), (CONCLUDE, "Commit…", true));

        dialog.update(cx, |dialog, cx| dialog.press(CONCLUDE, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.conflicts_dialog().is_none()), "Commit… closes the dialog");
        assert!(
            workbench.read_with(cx, |w, _| w.is_tool_window_visible(COMMIT_TOOL_WINDOW)),
            "Commit… shows the Commit tool window"
        );
    }

    /// A conflicted file whose versions break lines with CRLF is written back with CRLF: Accept Yours
    /// leaves our side's bytes on disk exactly, and Apply in the merge editor of another file writes
    /// its result with CRLF too.
    #[gpui_kit::test]
    async fn a_file_with_crlf_line_breaks_is_resolved_with_crlf(cx: &mut TestAppContext) {
        use crate::vcs::conflicts_dialog::{ACCEPT_YOURS, MERGE};
        use crate::vcs_diff::merge_gutters::{MergeControl, MergeSide};
        use crate::vcs_more::merge_actions::MergeCommand;
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            std::fs::write(root.join("a.txt"), "top\r\none\r\n").expect("the file is written");
            std::fs::write(root.join("b.txt"), "top\r\none\r\n").expect("the file is written");
            git_in(root, &["add", "."]);
            git_in(root, &["commit", "-q", "-m", "crlf"]);
            git_in(root, &["checkout", "-q", "-b", "feature"]);
            std::fs::write(root.join("a.txt"), "top\r\nfeature\r\n").expect("the file is written");
            std::fs::write(root.join("b.txt"), "top\r\nfeature\r\n").expect("the file is written");
            git_in(root, &["commit", "-q", "-am", "feature side"]);
            git_in(root, &["checkout", "-q", "main"]);
            std::fs::write(root.join("a.txt"), "top\r\nmain\r\n").expect("the file is written");
            std::fs::write(root.join("b.txt"), "top\r\nmain\r\n").expect("the file is written");
            git_in(root, &["commit", "-q", "-am", "main side"]);
        });
        let dialog = merge_into_conflicts(&workbench, cx);
        let names: Vec<String> =
            dialog.read_with(cx, |dialog, _| dialog.rows().into_iter().map(|row| row.name).collect());
        assert_eq!(names, ["a.txt", "b.txt"]);
        dialog.update(cx, |dialog, cx| dialog.press(ACCEPT_YOURS, cx));
        wait_long(cx, "a.txt is resolved", |cx| dialog.read_with(cx, |dialog, _| dialog.rows().len() == 1));
        assert_eq!(std::fs::read(root.join("a.txt")).expect("the file is read"), b"top\r\nmain\r\n");

        dialog.update(cx, |dialog, cx| dialog.press(MERGE, cx));
        let view = merge_view_loaded(&workbench, cx);
        let accept = MergeControl::Accept { index: 0, side: MergeSide::Right, resolve: true };
        view.update(cx, |view, cx| view.apply_control(accept, cx));
        view.update(cx, |view, cx| view.run(MergeCommand::Apply, cx));
        wait_long(cx, "the merge editor closes", |cx| workbench.read_with(cx, |w, _| w.merge_view().is_none()));
        assert_eq!(std::fs::read(root.join("b.txt")).expect("the file is read"), b"top\r\nfeature\r\n");
        assert_eq!(git_in(&root, &["ls-files", "-u"]), "", "both files are staged");
    }

    /// Merge… opens the three-way merge editor on the base with the conflict between both sides;
    /// accepting the left side for it and Apply write the resolved text without markers, stage the
    /// file and close the editor; committing then concludes the merge.
    #[gpui_kit::test]
    async fn merge_opens_the_three_way_editor_and_apply_writes_and_stages_the_resolved_file(cx: &mut TestAppContext) {
        use crate::vcs::conflicts_dialog::MERGE;
        use crate::vcs_diff::merge_gutters::{MergeControl, MergeSide};
        use crate::vcs_more::merge_actions::MergeCommand;
        let (workbench, cx, _folders, root) = git_project(cx, conflicting_branches);
        let dialog = merge_into_conflicts(&workbench, cx);
        dialog.update(cx, |dialog, cx| dialog.press(MERGE, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.conflicts_dialog().is_none()), "Merge… closes the dialog");
        let view = merge_view_loaded(&workbench, cx);
        let (result, changes) = view.read_with(cx, |view, _| (view.text(), view.changes().to_vec()));
        assert_eq!(result.as_deref(), Some("one\n"), "the result starts as the base");
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert!(changes[0].is_conflict());
        let sides = (changes[0].left.as_str(), changes[0].base.as_str(), changes[0].right.as_str());
        assert_eq!(sides, ("main\n", "one\n", "feature\n"));
        assert!(
            cx.update(|window, cx| workbench.read(cx).merge_editor_focused(window, cx)),
            "the merge editor has the keyboard"
        );

        let accept = MergeControl::Accept { index: 0, side: MergeSide::Left, resolve: true };
        view.update(cx, |view, cx| view.apply_control(accept, cx));
        assert_eq!(view.read_with(cx, |view, _| view.text()).as_deref(), Some("main\n"));
        assert_eq!(view.read_with(cx, |view, _| view.summary().pending), 0);
        view.update(cx, |view, cx| view.run(MergeCommand::Apply, cx));
        wait_long(cx, "the merge editor closes", |cx| workbench.read_with(cx, |w, _| w.merge_view().is_none()));
        let written = std::fs::read_to_string(root.join("a.txt")).expect("the file is read");
        assert_eq!(written, "main\n");
        assert!(!written.contains("<<<<<<<") && !written.contains(">>>>>>>"), "{written}");
        assert_eq!(git_in(&root, &["ls-files", "-u"]), "", "the file is staged");
        assert!(notification_titles(&workbench, cx).contains(&"a.txt: conflicts resolved".to_owned()));

        // Nothing is left to resolve: the title bar offers only Abort Merge, and a commit concludes it.
        let merge = workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).cloned()).expect("the merge is stopped");
        wait_long(cx, "the widget follows", |cx| merge.read_with(cx, |merge, cx| merge.labels(cx)) == ["Abort Merge"]);
        git_in(&root, &["commit", "-q", "--no-edit"]);
        workbench.update(cx, |w, cx| w.refresh_vcs(cx));
        wait_long(cx, "the merge is concluded", |cx| {
            workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).is_none())
        });
        // The branch menu merges the branch by its full reference, which git's message names.
        assert_eq!(git_in(&root, &["log", "-1", "--format=%s"]), "Merge branch 'refs/heads/feature'");
    }

    /// While the merge editor has the keyboard, Next and Previous Difference move its caret between
    /// the changes, and Apply All Non-Conflicting Changes, run as an action, takes both sides'
    /// changes and leaves the conflict; it is then disabled.
    #[gpui_kit::test]
    async fn the_merge_actions_move_between_the_changes_and_apply_the_non_conflicting_ones(cx: &mut TestAppContext) {
        use crate::platform::context::window_context;
        use crate::vcs::conflicts_dialog::MERGE;
        use crate::vcs_diff::merge_gutters::MergePane;
        let (workbench, cx, _folders, _root) = git_project(cx, |root| {
            init_repo(root);
            std::fs::write(root.join("b.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n").expect("the file is written");
            git_in(root, &["add", "."]);
            git_in(root, &["commit", "-q", "-m", "numbers"]);
            git_in(root, &["checkout", "-q", "-b", "feature"]);
            std::fs::write(root.join("b.txt"), "one\n2\n3\n4\nfive feature\n6\n7\n8\n9\n")
                .expect("the file is written");
            git_in(root, &["commit", "-q", "-am", "feature side"]);
            git_in(root, &["checkout", "-q", "main"]);
            std::fs::write(root.join("b.txt"), "1\n2\n3\n4\nfive main\n6\n7\n8\nnine\n").expect("the file is written");
            git_in(root, &["commit", "-q", "-am", "main side"]);
        });
        let dialog = merge_into_conflicts(&workbench, cx);
        dialog.update(cx, |dialog, cx| dialog.press(MERGE, cx));
        let view = merge_view_loaded(&workbench, cx);
        let starts: Vec<usize> =
            view.read_with(cx, |view, _| view.changes().iter().map(|change| change.from).collect());
        assert_eq!(starts.len(), 3, "theirs on line 1, the conflict on line 5, ours on line 9: {starts:?}");
        let caret = |cx: &mut VisualTestContext| view.read_with(cx, |view, _| view.selection(MergePane::Result).start);
        assert_eq!(caret(cx), 0);
        run_action(cx, "diff.nextDifference", None);
        assert_eq!(caret(cx), starts[1]);
        run_action(cx, "diff.nextDifference", None);
        assert_eq!(caret(cx), starts[2]);
        run_action(cx, "diff.prevDifference", None);
        assert_eq!(caret(cx), starts[1]);

        let enabled = |id: &'static str, cx: &mut VisualTestContext| {
            cx.update(|window, cx| window_context(window, cx).is_truthy(id))
        };
        assert!(enabled("mergeEditorFocus", cx));
        assert!(enabled("merge.applyAll", cx));
        assert!(!enabled("merge.annotate", cx), "blame is not shown in the side panes");
        run_action(cx, "merge.applyAll", None);
        let (text, summary) = view.read_with(cx, |view, _| (view.text(), view.summary()));
        assert_eq!(text.as_deref(), Some("one\n2\n3\n4\n5\n6\n7\n8\nnine\n"));
        assert_eq!((summary.pending, summary.conflicts), (1, 1));
        assert!(!enabled("merge.applyAll", cx), "nothing non-conflicting is left");
    }

    /// Once the last conflict of a merge is resolved, "All conflicts resolved" offers Commit… and Abort
    /// Merge; Abort Merge undoes the merge and leaves the repository clean.
    #[gpui_kit::test]
    async fn abort_merge_once_its_conflicts_are_resolved_leaves_the_repository_clean(cx: &mut TestAppContext) {
        use crate::vcs::conflicts_dialog::{ACCEPT_THEIRS, CLOSE};
        let (workbench, cx, _folders, root) = git_project(cx, conflicting_branches);
        let dialog = merge_into_conflicts(&workbench, cx);
        dialog.update(cx, |dialog, cx| dialog.press(ACCEPT_THEIRS, cx));
        wait_long(cx, "the conflicts are resolved", |cx| {
            notice_titled(&workbench, "All conflicts resolved", cx).is_some()
        });
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("the file is read"), "feature\n");
        let notice = notice_titled(&workbench, "All conflicts resolved", cx).expect("the resolution is told");
        let labels: Vec<&str> = notice.actions.iter().map(|action| action.label.as_str()).collect();
        assert_eq!(labels, ["Commit…", "Abort Merge"]);
        dialog.update(cx, |dialog, cx| dialog.press(CLOSE, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.conflicts_dialog().is_none()), "Close closes the dialog");

        let abort = EntryAction::Own(notice.actions[1].clone());
        workbench.update_in(cx, |w, window, cx| w.run_notification_action(abort, window, cx));
        wait_long(cx, "the abort", |cx| workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).is_none()));
        assert!(!root.join(".git/MERGE_HEAD").exists(), "the merge is aborted");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("the file is read"), "main\n");
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "", "the working tree is clean");
        assert_eq!(git_head(&root), "main");
        wait_for_notice(&workbench, "Merge aborted", cx);
    }

    /// A conflict raises the sticky "Merge conflicts" warning naming the file, with Resolve…, and the
    /// file gets the conflict banner; once it is resolved the warning's balloon goes, "All conflicts
    /// resolved" takes its place, and so does the banner.
    #[gpui_kit::test]
    async fn the_merge_conflicts_notification_appears_with_the_conflict_and_expires_once_it_is_resolved(
        cx: &mut TestAppContext,
    ) {
        use crate::vcs::conflicts_dialog::ACCEPT_YOURS;
        let (workbench, cx, _folders, root) = git_project(cx, conflicting_branches);
        let dialog = merge_into_conflicts(&workbench, cx);
        let notice = notice_titled(&workbench, "Merge conflicts", cx).expect("the conflicts are told");
        assert_eq!(notice.key.as_deref(), Some("vcs.conflicts"));
        assert_eq!(notice.level, NotificationLevel::Warning);
        assert_eq!(notice.body.as_deref(), Some("1 file has conflicts: a.txt"));
        assert!(notice.sticky, "the warning stays until the conflicts go");
        let actions: Vec<(&str, &str)> =
            notice.actions.iter().map(|action| (action.label.as_str(), action.command.as_str())).collect();
        assert_eq!(actions, [("Resolve…", "vcs.resolveConflicts")]);
        let file = root.join("a.txt").to_string_lossy().into_owned();
        let banner = workbench.read_with(cx, |w, _| w.conflict_banner_for(&file)).expect("the file has a banner");
        assert!(banner.read_with(cx, |banner, _| banner.banner().is_some()));

        dialog.update(cx, |dialog, cx| dialog.press(ACCEPT_YOURS, cx));
        wait_long(cx, "the conflicts are resolved", |cx| {
            notice_titled(&workbench, "All conflicts resolved", cx).is_some()
        });
        let shown = workbench
            .read_with(cx, |w, _| w.notifications().items().iter().any(|n| n.title == "Merge conflicts" && n.balloon));
        assert!(!shown, "the warning is expired");
        assert!(workbench.read_with(cx, |w, _| w.conflict_banner_for(&file).is_none()), "the banner goes");
    }

    /// The rest of the merge editor's wiring on a stopped rebase whose dialog was closed: the editor's
    /// banner opens the merge editor; its comparisons open the diff window over it, its menus the popup
    /// and its questions a dialog whose No keeps it; Automatically Apply is stored; Accept Yours
    /// resolves the file, and "All conflicts resolved" then offers Continue Rebase, which goes on.
    #[gpui_kit::test]
    async fn the_merge_editor_opened_from_the_banner_runs_its_actions_and_the_rebase_is_continued_from_the_notice(
        cx: &mut TestAppContext,
    ) {
        use crate::vcs::conflicts_dialog::CLOSE;
        use crate::vcs_diff::merge_gutters::{MergeControl, MergeSide};
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            conflicting_branches(root);
            git_in(root, &["checkout", "-q", "feature"]);
        });
        choose_in_branch_menu(&workbench, "main", "Rebase 'feature' onto 'main'", cx);
        wait_long(cx, "the Conflicts dialog", |cx| workbench.read_with(cx, |w, _| w.conflicts_dialog().is_some()));
        let dialog = workbench.read_with(cx, |w, _| w.conflicts_dialog()).expect("the dialog is open");
        dialog.update(cx, |dialog, cx| dialog.press(CLOSE, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.conflicts_dialog().is_none()));

        // The banner over the conflicted file's editor opens the merge editor.
        let file = root.join("a.txt");
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        assert!(!context_is_on(cx, "mergeEditorFocus"), "the text editor has the keyboard");
        click_on(cx, "conflict-banner-action-0");
        let view = merge_view_loaded(&workbench, cx);
        assert_eq!(view.read_with(cx, |view, _| view.path().to_owned()), file.to_string_lossy());
        assert!(cx.update(|window, cx| workbench.read(cx).merge_editor_focused(window, cx)));
        assert!(context_is_on(cx, "mergeEditorFocus"));

        // Compare's menu lists the comparisons; choosing one, once the menu gave the keyboard back to
        // the merge editor, opens the diff window over it; closing that gives the keyboard back.
        run_action(cx, "merge.compare", None);
        let menu = workbench.read_with(cx, |w, _| w.open_menu::<crate::vcs_more::merge_actions::MergeCommand>());
        let lines = menu.expect("the Compare menu is open").read_with(cx, |menu, _| menu.lines());
        assert_eq!(lines[0].get(2).map(|line| line.trim_start_matches("> ")), Some("Yours and Theirs"), "{lines:?}");
        click_on(cx, "menu-row-0-2");
        assert!(workbench.read_with(cx, |w, _| w.popup.is_none()), "choosing closes the menu");
        assert!(workbench.read_with(cx, |w, _| w.diff_dialog().is_some()), "the comparison is shown");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.diff_dialog().is_none()), "Escape closes the comparison");
        assert!(cx.update(|window, cx| workbench.read(cx).merge_editor_focused(window, cx)));

        // The settings menu opens in the popup slot; Escape closes it and the merge editor keeps the keys.
        run_action(cx, "merge.settings", None);
        assert!(workbench.read_with(cx, |w, _| w.popup.is_some()), "the menu is shown");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.popup.is_none()), "Escape closes the menu");
        assert!(
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy("mergeEditorFocus"))
        );
        run_action(cx, "merge.autoApply", None);
        let stored =
            cx.update(|_, cx| cx.global::<UiStorage>().get(super::conflicts::AUTO_APPLY_KEY).map(str::to_owned));
        assert_eq!(stored.as_deref(), Some("true"));
        run_action(cx, "merge.autoApply", None);
        // Reload replaces what is shown once the versions are read again; a decision made before then
        // would be dropped with the old versions.
        let loads = view.read_with(cx, |view, _| view.loads());
        run_action(cx, "merge.reload", None);
        wait_long(cx, "the versions are read again", |cx| view.read_with(cx, |view, _| view.loads() > loads));

        // Cancel after a change asks; No keeps the editor, OK closes it.
        let accept = MergeControl::Accept { index: 0, side: MergeSide::Left, resolve: false };
        view.update(cx, |view, cx| view.apply_control(accept, cx));
        assert!(view.read_with(cx, |view, _| view.is_modified()));
        run_action(cx, "merge.cancel", None);
        let question = wait_for_prompt(&workbench, cx);
        assert_eq!(question.read_with(cx, |dialog, _| dialog.title().to_owned()), "Cancel Merge");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.merge_view().is_some()), "No keeps the merge editor");
        assert!(
            cx.update(|window, cx| workbench.read(cx).merge_editor_focused(window, cx)),
            "the question gives the keyboard back to the merge editor"
        );
        run_action(cx, "merge.cancel", None);
        wait_for_prompt(&workbench, cx);
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the merge editor closes", |cx| workbench.read_with(cx, |w, _| w.merge_view().is_none()));
        assert_eq!(git_in(&root, &["ls-files", "-u"]).lines().count(), 3, "the file is still unmerged");

        // Accept Yours (the replayed commit's side) from a merge editor the banner opened: the rebase
        // waits for Continue.
        click_on(cx, "conflict-banner-action-0");
        merge_view_loaded(&workbench, cx);
        run_action(cx, "merge.acceptYours", None);
        // The editor closes once the resolution's own read is back; the notice may come before, from
        // the backend's event on the staged file.
        wait_long(cx, "the merge editor closes", |cx| workbench.read_with(cx, |w, _| w.merge_view().is_none()));
        wait_long(cx, "the conflicts are resolved", |cx| {
            notice_titled(&workbench, "All conflicts resolved", cx).is_some()
        });
        let notice = notice_titled(&workbench, "All conflicts resolved", cx).expect("the resolution is told");
        let labels: Vec<&str> = notice.actions.iter().map(|action| action.label.as_str()).collect();
        assert_eq!(labels, ["Continue Rebase", "Abort Rebase"]);
        assert!(root.join(".git/rebase-merge").exists() || root.join(".git/rebase-apply").exists());
        let go_on = EntryAction::Own(notice.actions[0].clone());
        workbench.update_in(cx, |w, window, cx| w.run_notification_action(go_on, window, cx));
        wait_long(cx, "the rebase goes on", |cx| {
            workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).is_none())
                && !root.join(".git/rebase-merge").exists()
                && !root.join(".git/rebase-apply").exists()
        });
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("the file is read"), "feature\n");
        assert_eq!(git_in(&root, &["log", "-2", "--format=%s"]), "feature side\nmain side");
        wait_for_notice(&workbench, "Rebase continued", cx);
    }

    /// A rebase that stops on a conflict opens the Conflicts dialog; once its last conflict is resolved
    /// there, the rebase continues by itself and the dialog closes.
    #[gpui_kit::test]
    async fn a_rebase_whose_last_conflict_is_resolved_from_the_dialog_continues_by_itself(cx: &mut TestAppContext) {
        use crate::vcs::conflicts_dialog::ACCEPT_YOURS;
        use crate::vcs_ops::operations::InProgress;
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            conflicting_branches(root);
            git_in(root, &["checkout", "-q", "feature"]);
        });
        choose_in_branch_menu(&workbench, "main", "Rebase 'feature' onto 'main'", cx);
        wait_long(cx, "the Conflicts dialog", |cx| workbench.read_with(cx, |w, _| w.conflicts_dialog().is_some()));
        let dialog = workbench.read_with(cx, |w, _| w.conflicts_dialog()).expect("the dialog is open");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.continue_when_resolved()), Some(InProgress::Rebase));

        // During a rebase Yours is the commit being replayed: feature's.
        dialog.update(cx, |dialog, cx| dialog.press(ACCEPT_YOURS, cx));
        wait_long(cx, "the rebase goes on", |cx| {
            workbench.read_with(cx, |w, cx| w.merge_rebase_widget(cx).is_none())
                && !root.join(".git/rebase-merge").exists()
                && !root.join(".git/rebase-apply").exists()
        });
        assert!(workbench.read_with(cx, |w, _| w.conflicts_dialog().is_none()), "the dialog closes by itself");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).expect("the file is read"), "feature\n");
        assert_eq!(git_head(&root), "feature");
        assert_eq!(git_in(&root, &["log", "-2", "--format=%s"]), "feature side\nmain side");
        wait_for_notice(&workbench, "Rebase continued", cx);
        assert!(notice_titled(&workbench, "All conflicts resolved", cx).is_none(), "nothing is left to offer");
    }

    /// The status bar lists its items in the order their priorities give: the language servers
    /// (5), the caret (20), the line separator (30), the encoding (40), the indentation (50) and the
    /// read-only attribute (55). Its context menu hides one and stores the choice under
    /// `appearance.statusBarWidgets`; View › Appearance › Status Bar Widgets shows it again.
    // src/workbench/statusBarWidgets.test.tsx › status bar widgets › toggles a widget from the status bar
    // context menu and persists it
    #[gpui_kit::test]
    async fn toggles_a_widget_from_the_status_bar_context_menu_and_persists_it(cx: &mut TestAppContext) {
        use crate::workbench::status_bar_widgets::SetVisible;
        use gpui_kit::{Modifiers, MouseButton, point, px};
        use std::collections::BTreeMap;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.run_until_parked();
        let bar = workbench.read_with(cx, |w, _| w.status_bar().clone());
        // Background work comes and goes while the project is indexed: it is left out.
        let shown = |cx: &mut VisualTestContext| -> Vec<String> {
            let (_, right) = cx.update(|window, cx| bar.read(cx).shown(window, cx));
            right.into_iter().filter(|id| id != status_items::TASKS).collect()
        };
        let expected = [
            status_items::LANGUAGE_SERVICES,
            status_items::CARET,
            status_items::LINE_ENDING,
            status_items::ENCODING,
            status_items::INDENT,
            status_items::READ_ONLY,
        ];
        assert_eq!(shown(cx), expected);
        let selectors = [
            "status:editor.caret",
            "status:editor.lineEnding",
            "status:editor.encoding",
            "status:editor.indent",
            "status:editor.readOnly",
        ];
        let mut edges = Vec::new();
        for selector in selectors {
            edges.push(cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn")).left());
        }
        assert!(edges.windows(2).all(|pair| pair[0] < pair[1]), "left to right: {edges:?}");
        // The bar draws what the window's own accessor says.
        let texts = cx.update(|window, cx| bar.read(cx).texts(window, cx));
        let items = workbench.read_with(cx, |w, cx| w.status_items(cx));
        assert!(texts.windows(3).any(|three| three == items.as_slice()), "{texts:?} {items:?}");

        // A right click on the free space at the bar's left end opens its menu.
        let bounds = cx.debug_bounds("status-bar").expect("the bar is drawn");
        let at = point(bounds.left() + px(4.0), bounds.center().y);
        cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        let menu = workbench.read_with(cx, |w, _| w.open_menu::<SetVisible>()).expect("the bar's menu is open");
        let labels: Vec<String> =
            menu.read_with(cx, |menu, _| menu.rows().iter().map(|row| row.label.clone()).collect());
        assert_eq!(
            labels,
            [
                "Language Services",
                "Line:Column Number",
                "Line Separator",
                "File Encoding",
                "Indentation",
                "Read-only Attribute"
            ]
        );
        let index = labels.iter().position(|label| label == "Line Separator").expect("listed");
        for _ in 0..index {
            cx.simulate_keystrokes("down");
        }
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let stored = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| crate::platform::settings::global(cx).appearance.status_bar_widgets)
        };
        assert_eq!(stored(cx), BTreeMap::from([(status_items::LINE_ENDING.to_owned(), false)]));
        assert!(!shown(cx).contains(&status_items::LINE_ENDING.to_owned()));
        assert!(cx.debug_bounds("status:editor.lineEnding").is_none());

        // View › Appearance › Status Bar Widgets lists the titled items, the hidden one unchecked, each
        // running the toggle with its id.
        let widgets = cx.update(|window, cx| {
            let dynamic = menu::state(window, cx).dynamic;
            dynamic.get(super::menu_spec::DYNAMIC_STATUS_BAR_WIDGETS).cloned().unwrap_or_default()
        });
        let separator = widgets.iter().find(|item| item.label == "Line Separator").expect("listed");
        assert_eq!(separator.action, "view.toggleStatusBarWidget");
        assert_eq!(separator.arg.as_deref(), Some(status_items::LINE_ENDING));
        assert_eq!(separator.checked, Some(false));
        let caret = widgets.iter().find(|item| item.label == "Line:Column Number").expect("listed");
        assert_eq!(caret.checked, Some(true));

        run_action(cx, "view.toggleStatusBarWidget", Some(serde_json::json!(status_items::LINE_ENDING)));
        assert_eq!(stored(cx), BTreeMap::from([(status_items::LINE_ENDING.to_owned(), true)]));
        assert_eq!(shown(cx), expected);
        assert!(cx.debug_bounds("status:editor.lineEnding").is_some());
    }

    // src/editor/core/view/statusItems.test.tsx (the line separator widget)
    #[gpui_kit::test]
    async fn a_click_on_the_line_separator_opens_its_menu_and_crlf_changes_the_files_separator(
        cx: &mut TestAppContext,
    ) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        let bar = workbench.read_with(cx, |w, _| w.status_bar().clone());
        click_on(cx, "status:editor.lineEnding");
        let menu = workbench.read_with(cx, |w, _| w.open_menu::<StatusCommand>()).expect("the menu is open");
        let labels: Vec<String> =
            menu.read_with(cx, |menu, _| menu.rows().iter().map(|row| row.label.clone()).collect());
        let crlf = labels.iter().position(|label| label.starts_with("CRLF")).expect("CRLF is offered");
        for _ in 0..crlf {
            cx.simulate_keystrokes("down");
        }
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(editor.read_with(cx, |editor, _| editor.status().line_ending), ide_text::LineEnding::CrLf);
        assert!(editor.read_with(cx, |editor, _| editor.is_dirty()));
        assert_eq!(workbench.read_with(cx, |w, cx| w.status_items(cx))[1], "CRLF");
        let texts = cx.update(|window, cx| bar.read(cx).texts(window, cx));
        assert!(texts.contains(&"CRLF".to_owned()), "the bar shows it too: {texts:?}");
    }

    /// The title bar of a project with a repository: the project and branch widgets on the left; the
    /// run widget, the IDE mode switch and Search Everywhere on the right, by priority, then the gear.
    /// Customize Main Toolbar takes the run widget off and stores the layout; Restore Defaults puts
    /// it back.
    // src/workbench/Workbench.test.tsx › status bar ordering › title items sort by priority within each side
    #[gpui_kit::test]
    async fn title_widgets_sort_by_priority_and_customize_toolbar_removes_and_restores_one(cx: &mut TestAppContext) {
        use super::main_toolbar::{PROJECT_WIDGET, SEARCH_ENTRY, Side, VCS_WIDGET};
        use super::title_bar::{IDE_MODE_ITEM, RUN_ITEM};
        let (workbench, cx, _folders, _root) = git_project(cx, init_repo);
        wait_long(cx, "the title widgets", |cx| {
            workbench.read_with(cx, |w, _| w.run_widget().is_some() && w.branch_widget().is_some())
        });
        cx.run_until_parked();
        let bar = workbench.read_with(cx, |w, _| w.title_bar().clone());
        let shown = |cx: &mut VisualTestContext| cx.update(|window, cx| bar.read(cx).shown(window, cx));
        let (left, right) = shown(cx);
        assert_eq!(left, [PROJECT_WIDGET, VCS_WIDGET]);
        assert_eq!(right, [RUN_ITEM, IDE_MODE_ITEM, SEARCH_ENTRY], "the Build hammer has nothing to build here");
        let mut edges = Vec::new();
        for selector in ["title:project", "title:vcs.branch", "title:run.widget", "title:app.ideMode", "title:gear"] {
            edges.push(cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn")).left());
        }
        assert!(edges.windows(2).all(|pair| pair[0] < pair[1]), "{edges:?}");

        run_action(cx, "toolbar.customize", None);
        let dialog = workbench.read_with(cx, |w, _| w.customize_toolbar_dialog().cloned()).expect("the dialog is open");
        let rows = dialog.read_with(cx, |dialog, _| dialog.rows(Side::Right));
        assert_eq!(rows, ["Build Project", "Run Widget", "IDE Mode", "Search Everywhere"]);
        click_on(cx, "customize-row:right:1");
        click_on(cx, "customize-remove");
        click_on(cx, "customize-ok");
        assert!(workbench.read_with(cx, |w, _| w.customize_toolbar_dialog().is_none()), "OK closes the dialog");
        let stored = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| crate::platform::settings::global(cx).appearance.main_toolbar)
        };
        let saved = stored(cx).expect("the layout is stored");
        assert_eq!(saved.hidden, [RUN_ITEM]);
        assert!(!saved.right.contains(&RUN_ITEM.to_owned()), "{saved:?}");
        assert_eq!(shown(cx).1, [IDE_MODE_ITEM, SEARCH_ENTRY]);
        assert!(cx.debug_bounds("title:run.widget").is_none());
        assert!(cx.debug_bounds("run-widget-chooser").is_none());

        run_action(cx, "toolbar.customize", None);
        click_on(cx, "customize-restore");
        click_on(cx, "customize-ok");
        let saved = stored(cx).expect("the layout is stored");
        assert!(saved.hidden.is_empty() && saved.right.contains(&RUN_ITEM.to_owned()), "{saved:?}");
        assert_eq!(shown(cx).1, [RUN_ITEM, IDE_MODE_ITEM, SEARCH_ENTRY]);
        assert!(cx.debug_bounds("title:run.widget").is_some());
    }

    // src/workbench/TitleBar.test.tsx › compact window header › shows the window title and follows it, without
    // toolbar widgets
    #[gpui_kit::test]
    async fn shows_the_window_title_and_follows_it_without_toolbar_widgets(cx: &mut TestAppContext) {
        use super::title_bar::TitleBarMode;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        cx.run_until_parked();
        let bar = workbench.read_with(cx, |w, _| w.title_bar().clone());
        let mode = |cx: &mut VisualTestContext| bar.read_with(cx, |bar, _| bar.mode());
        let shown_title = |cx: &mut VisualTestContext| bar.read_with(cx, |bar, _| bar.shown_title().map(str::to_owned));
        assert_eq!(mode(cx), TitleBarMode::Full);
        assert!(cx.debug_bounds("title:gear").is_some() && cx.debug_bounds("title:project").is_some());
        assert_eq!(shown_title(cx), None, "a project's full bar has no title of its own");

        run_action(cx, "view.toggleToolbar", None);
        assert_eq!(mode(cx), TitleBarMode::Compact);
        assert!(cx.debug_bounds("title:compact").is_some());
        for selector in ["title:gear", "title:project", "title:app.ideMode", "title:run.widget"] {
            assert!(cx.debug_bounds(selector).is_none(), "{selector}");
        }
        let title = workbench.read_with(cx, |w, cx| w.title(cx));
        assert_eq!(shown_title(cx), Some(title));

        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.run_until_parked();
        let title = workbench.read_with(cx, |w, cx| w.title(cx));
        assert!(title.ends_with("notes.txt"), "{title}");
        assert_eq!(shown_title(cx), Some(title), "it follows the window's title");

        run_action(cx, "view.toggleToolbar", None);
        assert_eq!(mode(cx), TitleBarMode::Full);
        assert!(cx.debug_bounds("title:gear").is_some());
    }

    /// The title bar of a project without a repository: no branch widget but "Version Control ▾" with
    /// Show Git Log; the project widget's and the gear's menus open in
    /// the popup slot; the bar's context menu takes the IDE mode switch off
    /// and Restore Default Toolbar brings it back; Customize Main Toolbar's Escape writes nothing. The
    /// Welcome screen's bar carries its title only.
    #[gpui_kit::test]
    async fn the_title_bars_menus_open_and_the_context_menu_hides_and_restores_an_entry(cx: &mut TestAppContext) {
        use super::main_toolbar::PROJECT_WIDGET;
        use super::title_bar::{IDE_MODE_ITEM, ToolbarCommand};
        use gpui_kit::{Modifiers, MouseButton, point};
        let _home = start(cx);
        let (_folder, _root, workbench, cx) = search_project(cx);
        cx.run_until_parked();
        let bar = workbench.read_with(cx, |w, _| w.title_bar().clone());
        let menu_labels = |cx: &mut VisualTestContext| -> Vec<String> {
            let menu = workbench.read_with(cx, |w, _| w.open_menu::<ToolbarCommand>()).expect("a menu is open");
            menu.read_with(cx, |menu, _| menu.rows().iter().map(|row| row.label.clone()).collect())
        };
        let close = |cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            assert!(workbench.read_with(cx, |w, _| w.popup.is_none()), "Escape closes the menu");
        };

        click_on(cx, "title:project");
        let rows = menu_labels(cx);
        assert_eq!(rows.first().map(String::as_str), Some("Open…"), "{rows:?}");
        assert_eq!(rows.last().map(String::as_str), Some("Close Project"), "{rows:?}");
        close(cx);
        // Without a repository there is no branch widget: "Version Control ▾" takes its place and offers
        // the registered actions of its list.
        let (left, _) = cx.update(|window, cx| bar.read(cx).shown(window, cx));
        assert_eq!(left, [PROJECT_WIDGET, "vcs.branch"], "Show Git Log is registered: the fallback is drawn");
        click_on(cx, "title:vcs.branch");
        let rows = menu_labels(cx);
        assert!(rows.contains(&"Show Git Log".to_owned()), "{rows:?}");
        close(cx);
        click_on(cx, "title:gear");
        assert!(menu_labels(cx).contains(&"Customize Main Toolbar".to_owned()), "{:?}", menu_labels(cx));
        close(cx);

        // The bar's context menu: a check per entry, then Customize Toolbar… and Restore Default Toolbar.
        let right_click = |cx: &mut VisualTestContext| {
            let bounds = cx.debug_bounds("title-bar").expect("the bar is drawn");
            let at = point(bounds.center().x, bounds.center().y);
            cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::default());
            cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::default());
            cx.run_until_parked();
        };
        let click_row = |label: &str, cx: &mut VisualTestContext| {
            let menu = workbench.read_with(cx, |w, _| w.open_menu::<ToolbarCommand>()).expect("a menu is open");
            let rows: Vec<String> =
                menu.read_with(cx, |menu, _| menu.rows().iter().map(|row| row.label.clone()).collect());
            let index = rows.iter().position(|row| row == label).unwrap_or_else(|| panic!("{label} is in {rows:?}"));
            // The separator before Customize Toolbar… counts as a line of the menu.
            let entries = rows.iter().position(|row| row == "Customize Toolbar…").expect("listed");
            let line = if index >= entries { index + 1 } else { index };
            let selector: &'static str = Box::leak(format!("menu-row-0-{line}").into_boxed_str());
            click_on(cx, selector);
        };
        let stored = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| crate::platform::settings::global(cx).appearance.main_toolbar)
        };
        right_click(cx);
        click_row("IDE Mode", cx);
        let saved = stored(cx).expect("the layout is stored");
        assert_eq!(saved.hidden, [IDE_MODE_ITEM]);
        assert!(cx.debug_bounds("title:app.ideMode").is_none(), "the switch is off the bar");
        right_click(cx);
        click_row("Restore Default Toolbar", cx);
        assert_eq!(stored(cx), None, "the default toolbar is stored as none");
        assert!(cx.debug_bounds("title:app.ideMode").is_some(), "the switch is back");

        // Customize Main Toolbar: Escape cancels without writing.
        run_action(cx, "toolbar.customize", None);
        assert!(workbench.read_with(cx, |w, _| w.customize_toolbar_dialog().is_some()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.customize_toolbar_dialog().is_none()), "Escape closes it");
        assert_eq!(stored(cx), None);

        // The IDE mode switch and Search Everywhere run their actions from the bar.
        let full = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, cx| w.ide_mode(cx).is_full());
        let before = full(cx);
        click_on(cx, "title:app.ideMode");
        assert_eq!(full(cx), !before, "a click switches the IDE mode");
        click_on(cx, "title:app.ideMode");
        assert_eq!(full(cx), before, "and back");
        click_on(cx, "title:action:navigate.searchEverywhere");
        assert!(workbench.read_with(cx, |w, _| w.search_everywhere().is_some()), "Search Everywhere opens");
        close(cx);

        // The Welcome screen: its title, no toolbar and no gear.
        workbench.update_in(cx, |w, window, cx| w.close_project(window, cx));
        cx.run_until_parked();
        assert_eq!(
            bar.read_with(cx, |bar, _| bar.shown_title().map(str::to_owned)).as_deref(),
            Some("Welcome to OpenIDEA")
        );
        assert!(cx.debug_bounds("title:window").is_some());
        assert!(cx.debug_bounds("title:gear").is_none() && cx.debug_bounds("title:project").is_none());
    }

    /// The window's own buttons say what they do when the pointer rests on them: a stripe button its
    /// tool window with the shortcut that shows it, a tool window's Hide with Hide Active Tool Window's
    /// shortcut, an editor tab's close button, the title bar's gear and Search Everywhere, and the
    /// status bar's caret with Go to Line's shortcut.
    // src/workbench/toolwindows/ToolWindows.tsx (StripeButton, Hide), src/ui/Tabs.tsx (Close Tab),
    // src/workbench/TitleBar.tsx (GearButton, ToolbarEntry)
    #[gpui_kit::test]
    async fn the_windows_buttons_show_their_tooltips(cx: &mut TestAppContext) {
        use crate::ui::tooltip::hovered_tooltip;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = search_project(cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.run_until_parked();
        assert!(hovered_tooltip(cx, "stripe:project", "Project", Some("toolwindow.project")));
        assert!(hovered_tooltip(cx, "hide:project", "Hide", Some("toolwindow.hideActive")));
        let tab =
            workbench.read_with(cx, |w, _| w.tabs().and_then(|tabs| tabs.pane(&tabs.active_pane)?.active.clone()));
        let tab = tab.expect("a tab");
        let close: &'static str = Box::leak(format!("close:{tab}").into_boxed_str());
        assert!(hovered_tooltip(cx, close, "Close Tab", None));
        assert!(hovered_tooltip(cx, "title:gear", "IDE and Project Settings", None));
        // Search Everywhere shows its binding, or the double Shift no binding can name.
        let bound = cx.update(|_, cx| {
            let keymap = &cx.global::<crate::platform::input::Keymap>().0;
            keymap.shortcut_label("navigate.searchEverywhere", crate::platform::keymap::is_mac_host())
        });
        let search = format!("Search Everywhere  {}", bound.as_deref().unwrap_or("⇧⇧"));
        assert!(hovered_tooltip(cx, "title:action:navigate.searchEverywhere", &search, None), "{search}");
        assert!(hovered_tooltip(cx, "status:editor.caret", "Go to Line:Column", Some("navigate.gotoLine")));
    }

    // ---------------------------------------------------------------------------------------------
    // The Project view's file operations

    fn project_tree_of(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Entity<ProjectTree> {
        workbench.read_with(cx, |w, _| w.project_tree().cloned()).expect("a tree")
    }

    fn text_of(path: &std::path::Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// Selects `path` in the tree and gives the tree the keyboard, in the active window.
    fn select_in_tree(workbench: &Entity<Workbench>, path: &std::path::Path, cx: &mut VisualTestContext) {
        let tree = project_tree_of(workbench, cx);
        let at = text_of(path);
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.is_loaded(tree.root())));
        assert!(tree.update(cx, |tree, cx| tree.reveal_path(&at, cx)), "{at} is in the project");
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.selected_path() == Some(at.as_str())));
        cx.update(|window, cx| {
            window.activate_window();
            let focus = tree.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
    }

    fn tree_labels(tree: &Entity<ProjectTree>, cx: &mut VisualTestContext) -> Vec<String> {
        tree.read_with(cx, |tree, _| tree.rows().iter().skip(1).map(|row| row.node.label.clone()).collect())
    }

    #[track_caller]
    fn new_popup_of(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::project_view::new_popup::NewPopupView> {
        workbench.read_with(cx, |w, _| w.new_popup()).expect("the New popup is open")
    }

    /// Types `name` into the New popup's name field, replacing what is there, and presses Enter.
    fn enter_new_name(
        popup: &Entity<crate::project_view::new_popup::NewPopupView>,
        name: &str,
        cx: &mut VisualTestContext,
    ) {
        let field = popup.read_with(cx, |popup, _| popup.name_field().clone());
        field.update(cx, |field, cx| field.set_text("", cx));
        cx.simulate_input(name);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    fn close_backend_project(cx: &mut VisualTestContext) {
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    /// New File in the Project view: the New popup for the selected folder, File, a name: the file
    /// is on disk, shown and selected in the tree and open in the editor. A name that is taken or
    /// invalid shows the model's problem and creates nothing.
    #[gpui_kit::test]
    async fn new_file_in_the_project_view_creates_selects_and_opens_the_file(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/main.rs", "fn main() {}\n")]);
        let tree = project_tree_of(&workbench, cx);
        select_in_tree(&workbench, &root.join("src"), cx);
        run_action(cx, "project.newFile", None);
        let popup = new_popup_of(&workbench, cx);
        assert_eq!(popup.read_with(cx, |popup, _| popup.rows())[..2], ["File", "Directory"]);
        cx.simulate_keystrokes("enter");
        assert_eq!(popup.read_with(cx, |popup, _| popup.naming().map(str::to_owned)), Some("File".to_owned()));

        // Taken, then invalid: the problem shows and nothing is created.
        enter_new_name(&popup, "main.rs", cx);
        settle(cx, |cx| popup.update(cx, |popup, cx| popup.name_error(cx)).is_some());
        assert_eq!(popup.update(cx, |popup, cx| popup.name_error(cx)), Some("'main.rs' already exists".to_owned()));
        enter_new_name(&popup, "a\0b", cx);
        assert!(popup.update(cx, |popup, cx| popup.name_error(cx)).is_some());
        assert!(workbench.read_with(cx, |w, _| w.new_popup()).is_some(), "the popup stays open");
        let listed = |root: &std::path::Path| std::fs::read_dir(root.join("src")).map(|dir| dir.count()).unwrap_or(0);
        assert_eq!(listed(&root), 1);

        enter_new_name(&popup, "notes.txt", cx);
        let notes = root.join("src/notes.txt");
        let at = text_of(&notes);
        settle(cx, |cx| {
            notes.is_file()
                && tree.read_with(cx, |tree, _| tree.selected_path() == Some(at.as_str()))
                && workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == notes.as_path()))
        });
        assert!(workbench.read_with(cx, |w, _| w.new_popup()).is_none(), "the popup closed");
        assert_eq!(tree_labels(&tree, cx), ["src", "main.rs", "notes.txt"]);
        close_backend_project(cx);
    }

    /// New Directory asks for the folder's name at once and creates it; File › New does the same
    /// for a file, and a nested name creates the folders on the way.
    #[gpui_kit::test]
    async fn new_directory_and_a_nested_new_file_create_their_folders(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/main.rs", "fn main() {}\n")]);
        let tree = project_tree_of(&workbench, cx);
        select_in_tree(&workbench, &root.join("src/main.rs"), cx);
        run_action(cx, "project.newFolder", None);
        let popup = new_popup_of(&workbench, cx);
        assert_eq!(popup.read_with(cx, |popup, _| popup.naming().map(str::to_owned)), Some("Directory".to_owned()));
        enter_new_name(&popup, "util", cx);
        settle(cx, |_| root.join("src/util").is_dir());
        let util = text_of(&root.join("src/util"));
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.selected_path() == Some(util.as_str())));

        run_action(cx, "file.new", Some(serde_json::json!(text_of(&root))));
        let popup = new_popup_of(&workbench, cx);
        assert_eq!(popup.read_with(cx, |popup, _| popup.naming().map(str::to_owned)), Some("File".to_owned()));
        enter_new_name(&popup, "a/b/c.txt", cx);
        let nested = root.join("a/b/c.txt");
        settle(cx, |cx| {
            nested.is_file()
                && workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == nested.as_path()))
        });
        let at = text_of(&nested);
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.selected_path() == Some(at.as_str())));
        close_backend_project(cx);
    }

    /// Rename… on a file: the Rename dialog, a new name, the preview of the text reference another
    /// file has to it, confirmed: the file is renamed on disk and in the tree, the reference is
    /// updated, the open editor follows (its tab is the new name, saving writes the new path).
    /// Undo Move / Rename brings everything back.
    #[gpui_kit::test]
    async fn rename_renames_the_file_its_reference_and_its_open_editor(cx: &mut TestAppContext) {
        use crate::project_view::file_dialogs::{FileDialog, RefactoringPreview};
        let _home = start(cx);
        let (_folder, root, workbench, cx) =
            trusted_project(cx, &[("docs/old.txt", "first\n"), ("README.md", "See old.txt for details.\n")]);
        let tree = project_tree_of(&workbench, cx);
        let old = root.join("docs/old.txt");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(old.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        select_in_tree(&workbench, &old, cx);
        run_action(cx, "project.rename", None);
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>().is_some()));
        let dialog = workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>()).expect("the Rename dialog");
        let field = dialog.read_with(cx, |dialog, _| dialog.name_field().cloned()).expect("a name field");
        assert_eq!(field.read_with(cx, |field, _| field.text().to_owned()), "old.txt");
        field.update(cx, |field, cx| field.set_text("new.txt", cx));
        cx.simulate_keystrokes("enter");
        // README.md mentions the file: the refactoring shows what it changes there first.
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.file_dialog::<RefactoringPreview>().is_some()));
        let preview = workbench.read_with(cx, |w, _| w.file_dialog::<RefactoringPreview>()).expect("a preview");
        assert_eq!(preview.read_with(cx, |preview, _| preview.file_paths()), [text_of(&root.join("README.md"))]);
        cx.simulate_keystrokes("enter");
        let new = root.join("docs/new.txt");
        settle(cx, |_| new.is_file() && !old.exists());
        assert_eq!(std::fs::read_to_string(root.join("README.md")).expect("read"), "See new.txt for details.\n");
        let at = text_of(&new);
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.selected_path() == Some(at.as_str())));
        assert!(tree_labels(&tree, cx).contains(&"new.txt".to_owned()));
        assert!(!tree_labels(&tree, cx).contains(&"old.txt".to_owned()));
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == new.as_path()))
        });
        assert_eq!(workbench.read_with(cx, |w, _| w.tab_titles()[0].name.clone()), "new.txt");
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        cx.update(|window, cx| {
            let focus = editor.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_input("x");
        cx.simulate_keystrokes("cmd-s");
        settle(cx, |_| std::fs::read_to_string(&new).is_ok_and(|text| text == "xfirst\n"));
        assert!(!old.exists(), "saving writes the new path only");
        assert!(notification_titles(&workbench, cx).contains(&"Rename 'old.txt' to 'new.txt'".to_owned()));

        // The backend moves the file back, then reverts the reference.
        run_action(cx, "project.undoMove", None);
        let readme = root.join("README.md");
        settle(cx, |_| {
            old.is_file()
                && !new.exists()
                && std::fs::read_to_string(&readme).is_ok_and(|text| text == "See old.txt for details.\n")
        });
        settle(cx, |cx| tree_labels(&tree, cx).contains(&"old.txt".to_owned()));
        close_backend_project(cx);
    }

    /// Move… puts the file into the folder typed in the Move dialog; Copy… copies it under the name
    /// the Copy dialog is given.
    #[gpui_kit::test]
    async fn move_and_copy_go_through_their_dialogs(cx: &mut TestAppContext) {
        use crate::project_view::file_dialogs::FileDialog;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/data.json", "{}\n"), ("lib/keep.txt", "")]);
        let tree = project_tree_of(&workbench, cx);
        let data = root.join("src/data.json");
        select_in_tree(&workbench, &data, cx);
        run_action(cx, "project.move", None);
        let dialog = workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>()).expect("the Move dialog");
        let target = dialog.read_with(cx, |dialog, _| dialog.target_field().cloned()).expect("a target field");
        assert_eq!(target.read_with(cx, |field, _| field.text().to_owned()), text_of(&root.join("src")));
        target.update(cx, |field, cx| field.set_text(&text_of(&root.join("lib")), cx));
        cx.simulate_keystrokes("enter");
        let moved = root.join("lib/data.json");
        settle(cx, |_| moved.is_file() && !data.exists());
        let at = text_of(&moved);
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.selected_path() == Some(at.as_str())));
        assert!(workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>()).is_none());

        cx.update(|window, cx| {
            let focus = tree.focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.run_until_parked();
        run_action(cx, "project.copyElement", None);
        let dialog = workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>()).expect("the Copy dialog");
        let name = dialog.read_with(cx, |dialog, _| dialog.name_field().cloned()).expect("a name field");
        name.update(cx, |field, cx| field.set_text("copy.json", cx));
        cx.simulate_keystrokes("enter");
        let copy = root.join("lib/copy.json");
        settle(cx, |_| copy.is_file());
        assert_eq!(std::fs::read_to_string(&copy).expect("read"), "{}\n");
        assert!(moved.is_file(), "the original stays");
        settle(cx, |cx| tree_labels(&tree, cx).contains(&"copy.json".to_owned()));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>().is_none()));
        close_backend_project(cx);
    }

    /// Delete… asks first; cancelling keeps the file, its row and its editor tab.
    #[gpui_kit::test]
    async fn delete_asks_and_cancelling_keeps_everything(cx: &mut TestAppContext) {
        use crate::project_view::file_dialogs::DeleteDialog;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/gone.rs", "fn a() {}\n")]);
        let tree = project_tree_of(&workbench, cx);
        let file = root.join("src/gone.rs");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        select_in_tree(&workbench, &file, cx);
        run_action(cx, "project.delete", None);
        let dialog = workbench.read_with(cx, |w, _| w.file_dialog::<DeleteDialog>()).expect("the Delete dialog");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.question().message.clone()), "Delete 'gone.rs'?");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.checked()), Some(true), "safe delete starts as stored");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.file_dialog::<DeleteDialog>()).is_none());
        assert!(file.is_file());
        assert!(tree_labels(&tree, cx).contains(&"gone.rs".to_owned()));
        assert_eq!(workbench.read_with(cx, |w, _| w.tab_titles().len()), 1);
        assert!(cx.update(|window, cx| tree.focus_handle(cx).is_focused(window)), "the tree has the keyboard back");
        close_backend_project(cx);
    }

    /// Delete… confirmed runs the safe delete's usage search, then the delete, and stores the choice. The
    /// file is removed before the answer, so the backend refuses it before it reaches the Trash, and the
    /// failure is reported.
    #[gpui_kit::test]
    async fn a_confirmed_delete_searches_deletes_and_reports_what_could_not_be_deleted(cx: &mut TestAppContext) {
        use crate::project_view::file_dialogs::DeleteDialog;
        use crate::project_view::operations::SAFE_DELETE_KEY;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("docs/notes.txt", "a\n"), ("docs/keep.txt", "")]);
        let file = root.join("docs/notes.txt");
        select_in_tree(&workbench, &file, cx);
        run_action(cx, "project.delete", None);
        assert!(workbench.read_with(cx, |w, _| w.file_dialog::<DeleteDialog>()).is_some());
        std::fs::remove_file(&file).expect("the file is removed");
        cx.simulate_keystrokes("enter");
        settle(cx, |cx| notification_titles(&workbench, cx).contains(&"Some items could not be deleted".to_owned()));
        let stored = cx.update(|_, cx| cx.global::<UiStorage>().get(SAFE_DELETE_KEY).map(str::to_owned));
        assert_eq!(stored.as_deref(), Some("true"));
        assert!(root.join("docs/keep.txt").is_file());
        close_backend_project(cx);
    }

    /// Copy in the tree, then Paste on another folder: the Copy dialog for the copied file, whose
    /// copy lands there. Cut and Paste goes through the Move dialog.
    #[gpui_kit::test]
    async fn copy_and_paste_in_the_tree_go_through_the_copy_and_move_dialogs(cx: &mut TestAppContext) {
        use crate::project_view::file_dialogs::FileDialog;
        use crate::project_view::operations::FileRefactorRequest;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/a.txt", "a\n"), ("lib/keep.txt", "")]);
        let file = root.join("src/a.txt");
        select_in_tree(&workbench, &file, cx);
        run_action(cx, "project.copy", None);
        select_in_tree(&workbench, &root.join("lib"), cx);
        run_action(cx, "project.paste", None);
        let dialog = workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>()).expect("the Copy dialog");
        let lib = text_of(&root.join("lib"));
        let request = dialog.read_with(cx, |dialog, _| dialog.request().clone());
        assert!(matches!(&request, FileRefactorRequest::Copy { target, .. } if *target == lib), "{request:?}");
        cx.simulate_keystrokes("enter");
        settle(cx, |_| root.join("lib/a.txt").is_file());
        assert!(file.is_file());
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>().is_none()));

        select_in_tree(&workbench, &file, cx);
        run_action(cx, "project.cut", None);
        select_in_tree(&workbench, &root.join("lib"), cx);
        run_action(cx, "project.paste", None);
        let dialog = workbench.read_with(cx, |w, _| w.file_dialog::<FileDialog>()).expect("the Move dialog");
        let request = dialog.read_with(cx, |dialog, _| dialog.request().clone());
        assert!(matches!(request, FileRefactorRequest::Move { .. }), "{request:?}");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(file.is_file(), "a cancelled move keeps the file");
        close_backend_project(cx);
    }

    /// Copy Path/Reference… lists the path forms of the edited file with the caret's line; Enter
    /// copies the one chosen. Copy Absolute Path in the tree copies the selection's path.
    #[gpui_kit::test]
    async fn copy_path_reference_lists_the_forms_of_the_edited_file_and_copies_one(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/notes.txt", "one\ntwo\n")]);
        let file = root.join("src/notes.txt");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(file.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.update(|window, _| window.activate_window());
        run_action(cx, "editor.copyPathReference", None);
        let popup = workbench.read_with(cx, |w, _| w.copy_path_popup()).expect("the Copy Path popup");
        let lines = popup.read_with(cx, |popup, _| popup.lines());
        let path = text_of(&file);
        let labels: Vec<&str> = lines.iter().map(|line| line.split(" | ").next().unwrap_or_default()).collect();
        assert_eq!(
            labels,
            ["Absolute Path", "File Name", "Path From Content Root", "Path with line number", "Reference"]
        );
        assert!(lines[0].starts_with(&format!("Absolute Path | {path} |")), "{lines:?}");
        assert!(lines[3].starts_with("Path with line number | src/notes.txt:1 |"), "{lines:?}");
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.copy_path_popup()).is_none());
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some("src/notes.txt".to_owned()));
        assert_eq!(editor_hint(&workbench, cx).as_deref(), Some("Copied src/notes.txt"));

        select_in_tree(&workbench, &file, cx);
        run_action(cx, "project.copyAbsolutePath", None);
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), Some(path));
        close_backend_project(cx);
    }

    /// A right click on a file shows the rows the model gives the selection; New › File from it
    /// creates in the file's folder. New in This Directory opens the New popup for the edited file.
    #[gpui_kit::test]
    async fn the_tree_context_menu_offers_the_model_rows_and_creates_from_new(cx: &mut TestAppContext) {
        use crate::project_view::context_menu::ProjectCommand;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("src/lib.rs", "pub fn a() {}\n")]);
        let file = root.join("src/lib.rs");
        select_in_tree(&workbench, &file, cx);
        let selector: &'static str = Box::leak(format!("project-row:{}", text_of(&file)).into_boxed_str());
        let row = cx.debug_bounds(selector).expect("the row is drawn");
        cx.simulate_mouse_down(row.center(), gpui_kit::MouseButton::Right, gpui_kit::Modifiers::default());
        cx.simulate_mouse_up(row.center(), gpui_kit::MouseButton::Right, gpui_kit::Modifiers::default());
        cx.run_until_parked();
        let menu = workbench.read_with(cx, |w, _| w.open_menu::<ProjectCommand>()).expect("the context menu is open");
        let lines = menu.read_with(cx, |menu, _| menu.lines());
        for label in ["New", "Cut", "Copy", "Copy Path/Reference…", "Paste", "Jump to Source", "Refactor", "Delete…"]
        {
            assert!(lines[0].iter().any(|line| line.trim_start_matches("> ") == label), "{label} in {lines:?}");
        }
        // New is selected first: Right opens its submenu, Enter chooses File.
        assert_eq!(lines[0][0], "> New");
        cx.simulate_keystrokes("right enter");
        cx.run_until_parked();
        let popup = new_popup_of(&workbench, cx);
        assert_eq!(popup.read_with(cx, |popup, _| popup.naming().map(str::to_owned)), Some("File".to_owned()));
        enter_new_name(&popup, "extra.rs", cx);
        let extra = root.join("src/extra.rs");
        settle(cx, |cx| {
            extra.is_file()
                && workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == extra.as_path()))
        });

        // In the editor, New in This Directory is for the edited file's folder.
        cx.run_until_parked();
        run_action(cx, "project.newInThisDirectory", None);
        let popup = new_popup_of(&workbench, cx);
        cx.simulate_keystrokes("enter");
        enter_new_name(&popup, "more.rs", cx);
        settle(cx, |_| root.join("src/more.rs").is_file());
        close_backend_project(cx);
    }

    /// The changes tree and the message box of the Commit tool window.
    pub(super) fn commit_parts(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> (Entity<crate::vcs::commit_view::CommitPanel>, Entity<crate::vcs::commit_box::CommitBox>) {
        let panel = workbench
            .read_with(cx, |w, _| w.tool_window_view(COMMIT_TOOL_WINDOW))
            .and_then(|view| view.downcast::<crate::vcs::commit_view::CommitPanel>().ok())
            .expect("the Commit window holds the changes");
        let commit_box = workbench.read_with(cx, |w, _| w.commit_box().cloned()).expect("the window has the box");
        (panel, commit_box)
    }

    fn commit_message(commit_box: &Entity<crate::vcs::commit_box::CommitBox>, cx: &mut VisualTestContext) -> String {
        commit_box.read_with(cx, |commit_box, cx| commit_box.message_area().read(cx).text().to_owned())
    }

    fn message_focused(commit_box: &Entity<crate::vcs::commit_box::CommitBox>, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| commit_box.read(cx).message_area().focus_handle(cx).is_focused(window))
    }

    fn set_clear_initial_message(on: bool, cx: &mut VisualTestContext) {
        let write = crate::platform::settings::SettingsWrite {
            key: "vcs.commit.clearInitialMessage",
            value: serde_json::Value::Bool(on),
        };
        cx.update(|_, cx| crate::platform::settings::set_global("w1", &[write], cx)).expect("the setting is stored");
    }

    /// ⌘↩ on macOS, Ctrl+Enter elsewhere.
    const COMMIT_KEYS: &str = if cfg!(target_os = "macos") { "cmd-enter" } else { "ctrl-enter" };

    /// With two changed files and one left out with its checkbox, Commit without a message commits
    /// nothing and says so at the field; with a typed message Commit commits exactly the included
    /// file with that message (`git log` and `git status` agree), the field clears (Clear initial
    /// commit message on), the tree shows only the file left out, the commit is reported and the
    /// message is in the history. A second commit from the keyboard keeps its message in the field,
    /// as the setting is off by default.
    #[gpui_kit::test]
    async fn commit_takes_exactly_the_included_files_with_the_typed_message(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            std::fs::write(root.join("b.txt"), "bee\n").expect("the file is written");
            git_in(root, &["add", "."]);
            git_in(root, &["commit", "-q", "-m", "second"]);
            std::fs::write(root.join("a.txt"), "one changed\n").expect("the file is written");
            std::fs::write(root.join("b.txt"), "bee changed\n").expect("the file is written");
        });
        set_clear_initial_message(true, cx);
        run_action(cx, "vcs.commit", None);
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(COMMIT_TOOL_WINDOW)));
        let (panel, commit_box) = commit_parts(&workbench, cx);
        wait_long(cx, "the changes are listed", |cx| {
            panel.read_with(cx, |panel, _| panel.lines()) == ["Changes (2)", "  a.txt", "  b.txt"]
        });
        assert!(message_focused(&commit_box, cx), "Commit… puts the keyboard in the message");
        assert_eq!(panel.read_with(cx, |panel, cx| panel.included_files(cx)), ["a.txt", "b.txt"]);

        // b.txt is left out with its checkbox.
        click_on(cx, "commit-include-b.txt");
        assert_eq!(panel.read_with(cx, |panel, cx| panel.included_files(cx)), ["a.txt"]);
        wait_long(cx, "the box counts one file", |cx| commit_box.read_with(cx, |b, _| b.input().included == 1));

        // Without a message nothing is committed.
        click_on(cx, "commit-commit");
        assert!(commit_box.read_with(cx, |commit_box, _| commit_box.empty_error()));
        assert!(message_focused(&commit_box, cx), "the field takes the keyboard");
        assert_eq!(git_in(&root, &["rev-list", "--count", "HEAD"]), "2");

        cx.simulate_input("Change a");
        click_on(cx, "commit-commit");
        wait_long(cx, "the commit is reported", |cx| {
            notification_titles(&workbench, cx).contains(&"1 file committed: Change a".to_owned())
        });
        assert_eq!(git_in(&root, &["log", "-1", "--format=%s"]), "Change a");
        assert_eq!(git_in(&root, &["show", "--name-only", "--format=", "HEAD"]), "a.txt");
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "M b.txt");
        wait_long(cx, "the tree follows", |cx| {
            panel.read_with(cx, |panel, _| panel.lines()) == ["Changes (1)", "  b.txt"]
        });
        assert_eq!(commit_message(&commit_box, cx), "", "the field clears");
        assert!(commit_box.read_with(cx, |commit_box, _| !commit_box.input().busy));
        run_action(cx, "vcs.showMessageHistory", None);
        let history = commit_box.read_with(cx, |commit_box, _| commit_box.history_lines());
        assert_eq!(history, Some(vec!["Change a".to_owned()]));
        // Escape in the message field closes the history.
        run_action(cx, "vcs.commit", None);
        cx.simulate_keystrokes("escape");
        assert_eq!(commit_box.read_with(cx, |commit_box, _| commit_box.history_lines()), None);

        // The file left out stays left out until it is included again, then ⌘↩ commits it.
        assert!(panel.read_with(cx, |panel, cx| panel.included_files(cx)).is_empty());
        set_clear_initial_message(false, cx);
        click_on(cx, "commit-include-b.txt");
        run_action(cx, "vcs.commit", None);
        assert!(message_focused(&commit_box, cx));
        cx.simulate_input("Change b");
        cx.simulate_keystrokes(COMMIT_KEYS);
        wait_long(cx, "the commit from the keyboard is reported", |cx| {
            notification_titles(&workbench, cx).contains(&"1 file committed: Change b".to_owned())
        });
        assert_eq!(git_in(&root, &["log", "-1", "--format=%s"]), "Change b");
        assert_eq!(git_in(&root, &["show", "--name-only", "--format=", "HEAD"]), "b.txt");
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "");
        wait_long(cx, "the tree is empty", |cx| panel.read_with(cx, |panel, _| panel.lines()).is_empty());
        assert_eq!(commit_message(&commit_box, cx), "Change b", "the message stays for the next commit");
        let history = cx.update(|_, cx| {
            let key = crate::vcs_ops::commit_store::history_key(Some(&root.to_string_lossy()));
            cx.global::<UiStorage>().get(&key).map(str::to_owned)
        });
        assert_eq!(history.as_deref(), Some(r#"["Change b","Change a"]"#));

        // Commit and Push… commits with the message in the field, then shows the Push dialog.
        std::fs::write(root.join("a.txt"), "one changed again\n").expect("the file is written");
        wait_long(cx, "the new change", |cx| commit_box.read_with(cx, |b, _| b.input().included == 1));
        run_action(cx, "vcs.commitAndPush", None);
        wait_long(cx, "the Push dialog", |cx| workbench.read_with(cx, |w, _| w.push_dialog().is_some()));
        assert_eq!(git_in(&root, &["rev-list", "--count", "HEAD"]), "5");
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "");
        cx.simulate_keystrokes("escape");
        assert!(workbench.read_with(cx, |w, _| w.push_dialog().is_none()), "Escape pushes nothing");
    }

    /// Amend puts the last commit's message in the field; edited and committed, it replaces the last
    /// commit with one holding the new change and the new message.
    #[gpui_kit::test]
    async fn amend_loads_the_last_message_and_amends_the_commit(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            std::fs::write(root.join("a.txt"), "one amended\n").expect("the file is written");
        });
        run_action(cx, "vcs.commit", None);
        let (panel, commit_box) = commit_parts(&workbench, cx);
        wait_long(cx, "the change is listed", |cx| panel.read_with(cx, |panel, _| panel.lines().len()) == 2);
        cx.simulate_input("draft");
        click_on(cx, "commit-amend");
        wait_long(cx, "the last message", |cx| commit_message(&commit_box, cx) == "first");
        assert_eq!(commit_box.update(cx, |commit_box, cx| commit_box.buttons(cx))[0].label, "Amend Commit");
        run_action(cx, "vcs.commit", None);
        cx.simulate_keystrokes(if cfg!(target_os = "macos") { "cmd-a" } else { "ctrl-a" });
        cx.simulate_input("first, amended");
        click_on(cx, "commit-commit");
        wait_long(cx, "the amend is reported", |cx| {
            notification_titles(&workbench, cx).contains(&"1 file amended: first, amended".to_owned())
        });
        assert_eq!(git_in(&root, &["log", "-1", "--format=%s"]), "first, amended");
        assert_eq!(git_in(&root, &["rev-list", "--count", "HEAD"]), "1", "the commit was replaced");
        assert_eq!(git_in(&root, &["show", "HEAD:a.txt"]), "one amended");
        assert_eq!(git_in(&root, &["status", "--porcelain"]), "");
        assert!(commit_box.read_with(cx, |commit_box, cx| !commit_box.message_area().read(cx).is_disabled()));
        let amend = workbench.update(cx, |w, cx| w.commit_state(cx).read(cx).amend);
        assert!(!amend, "Amend is off after the commit");
    }

    /// Selecting a commit in the Git tool window shows its message, author and changed files beside
    /// the log; Enter on a changed file shows the diff of the commit's change to it, and the menu's
    /// Show Diff does too.
    #[gpui_kit::test]
    async fn a_selected_commit_shows_its_details_and_the_diff_of_a_changed_file(cx: &mut TestAppContext) {
        use crate::vcs::log_view::GitLog;
        let (workbench, cx, _folders, _root) = git_project(cx, |root| {
            init_repo(root);
            std::fs::write(root.join("a.txt"), "two\n").expect("the file is written");
            std::fs::write(root.join("c.txt"), "new\n").expect("the file is written");
            git_in(root, &["add", "."]);
            git_in(root, &["commit", "-q", "-m", "Second change\n\nWith a body"]);
        });
        run_action(cx, "toolwindow.git", None);
        let log = workbench
            .read_with(cx, |w, _| w.tool_window_view(GIT_TOOL_WINDOW))
            .and_then(|view| view.downcast::<GitLog>().ok())
            .expect("the Git window holds the log");
        wait_long(cx, "the log is read", |cx| log.read_with(cx, |log, _| log.commits().len()) == 2);
        let pane = workbench.read_with(cx, |w, _| w.log_details().cloned()).expect("the window has the details");
        assert!(cx.debug_bounds("log-details-view-options").is_none(), "nothing is selected yet");
        cx.update(|window, cx| {
            let focus = log.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("down");
        wait_long(cx, "the changed files", |cx| {
            pane.read_with(cx, |pane, _| !pane.is_loading() && pane.lines().len() == 2)
        });
        assert_eq!(pane.read_with(cx, |pane, _| pane.lines()), ["a.txt", "c.txt"]);
        let message = pane.update(cx, |pane, cx| pane.message_lines(cx));
        assert_eq!(message.first().map(String::as_str), Some("Second change"), "{message:?}");
        assert!(message.iter().any(|line| line == "With a body"), "{message:?}");
        assert!(message.iter().any(|line| line.contains("Ada <ada@example.test>")), "{message:?}");
        assert!(cx.debug_bounds("log-details-view-options").is_some(), "the details are drawn beside the log");

        // Enter on a.txt shows the commit's change to it.
        cx.update(|window, cx| {
            let focus = pane.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("down enter");
        let dialog = workbench.read_with(cx, |w, _| w.diff_dialog().cloned()).expect("the diff is shown");
        let viewer = dialog.read_with(cx, |dialog, _| dialog.viewer().clone());
        wait_long(cx, "the diff's texts", |cx| viewer.read_with(cx, |viewer, _| viewer.difference_count() == 1));
        let changed = |lines: Vec<crate::vcs_diff::view::PaneLine>| -> Vec<String> {
            lines.into_iter().filter(|line| line.kind.is_some()).map(|line| line.text).collect()
        };
        assert_eq!(changed(viewer.read_with(cx, |viewer, _| viewer.left_lines())), ["one"]);
        assert_eq!(changed(viewer.read_with(cx, |viewer, _| viewer.right_lines())), ["two"]);
        assert!(dialog.read_with(cx, |dialog, _| dialog.data().path.ends_with("a.txt")));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.diff_dialog().is_none()));

        // The context menu's Show Diff on c.txt, an added file.
        let changes = pane.read_with(cx, |pane, _| pane.all_changes().to_vec());
        workbench.update_in(cx, |w, window, cx| {
            let added = changes.iter().filter(|change| change.path == "c.txt").cloned().collect();
            w.run_log_changes_command(crate::vcs::log_details::ChangesCommand::ShowDiff, added, window, cx);
        });
        let dialog = workbench.read_with(cx, |w, _| w.diff_dialog().cloned()).expect("the diff is shown");
        assert!(dialog.read_with(cx, |dialog, _| dialog.data().path.ends_with("c.txt")));
    }

    /// Typing in the Git log's text filter reduces the list to the matching commits, and a double
    /// click on a branch in the Branches pane shows that branch's log.
    #[gpui_kit::test]
    async fn the_log_filters_by_text_and_by_the_branch_chosen_in_the_branches_pane(cx: &mut TestAppContext) {
        use crate::vcs::log_view::GitLog;
        let (workbench, cx, _folders, _root) = git_project(cx, |root| {
            init_repo(root);
            let commit = |file: &str, message: &str| {
                std::fs::write(root.join(file), message).expect("the file is written");
                git_in(root, &["add", "."]);
                git_in(root, &["commit", "-q", "-m", message]);
            };
            commit("b.txt", "alpha fix");
            git_in(root, &["checkout", "-q", "-b", "feature"]);
            commit("f.txt", "on feature");
            git_in(root, &["checkout", "-q", "main"]);
            commit("c.txt", "beta change");
        });
        run_action(cx, "toolwindow.git", None);
        let log = workbench
            .read_with(cx, |w, _| w.tool_window_view(GIT_TOOL_WINDOW))
            .and_then(|view| view.downcast::<GitLog>().ok())
            .expect("the Git window holds the log");
        let subjects = |cx: &mut VisualTestContext| -> Vec<String> {
            log.read_with(cx, |log, _| log.commits().iter().map(|commit| commit.summary.clone()).collect())
        };
        wait_long(cx, "the log is read", |cx| subjects(cx).contains(&"beta change".to_owned()));
        // Without a branch filter the log shows every branch.
        assert_eq!(subjects(cx), ["beta change", "on feature", "alpha fix", "first"]);

        // The text filter applies once the typing rests.
        let bar = log.read_with(cx, |log, _| log.filter_bar().cloned()).expect("the log has the filter bar");
        cx.update(|window, cx| {
            let focus = bar.read(cx).text_field().focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_input("alpha");
        wait_long(cx, "the text filter", |cx| subjects(cx) == ["alpha fix"]);
        assert_eq!(log.read_with(cx, |log, _| log.filters().text.clone()), "alpha");
        for _ in 0.."alpha".len() {
            cx.simulate_keystrokes("backspace");
        }
        wait_long(cx, "the filter cleared", |cx| subjects(cx).len() == 4);

        // A double click on feature in the Branches pane shows feature's log.
        let pane = log.read_with(cx, |log, _| log.branches_pane().cloned()).expect("the log has the Branches pane");
        wait_long(cx, "the branches are listed", |cx| {
            pane.read_with(cx, |pane, _| pane.rows().iter().any(|row| row.label == "feature"))
        });
        let index = pane.read_with(cx, |pane, _| pane.rows().iter().position(|row| row.label == "feature"));
        let selector: &'static str =
            Box::leak(format!("branches-row-{}", index.expect("feature is listed")).into_boxed_str());
        cx.run_until_parked();
        let position = cx.debug_bounds(selector).expect("the row is drawn").center();
        let (button, modifiers) = (gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
        cx.simulate_event(gpui_kit::MouseDownEvent { position, modifiers, button, click_count: 2, first_mouse: false });
        cx.simulate_event(gpui_kit::MouseUpEvent { position, modifiers, button, click_count: 2 });
        wait_long(cx, "the branch's log", |cx| subjects(cx) == ["on feature", "alpha fix", "first"]);
        assert_eq!(log.read_with(cx, |log, _| log.filters().branches.clone()), ["feature"]);

        // Show Branches hides the pane and shows it again, and the choice is stored.
        run_action(cx, "vcs.log.showBranches", None);
        assert!(cx.debug_bounds(selector).is_none(), "the pane is hidden");
        let stored =
            cx.update(|_, cx| cx.global::<UiStorage>().get(crate::vcs::log_options::STORAGE_KEY).map(str::to_owned));
        assert!(stored.is_some_and(|json| json.contains("\"showBranches\":false")));
        run_action(cx, "vcs.log.showBranches", None);
        assert!(cx.debug_bounds(selector).is_some(), "the pane shows again");
    }

    /// A commit the pre-commit hook refuses is reported as failed and keeps its message; with Run
    /// Git hooks turned off in the options it goes through. A before-commit check ticked in the
    /// options is written to the settings.
    #[gpui_kit::test]
    async fn a_refused_commit_keeps_its_message_and_the_options_reach_the_commit(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = git_project(cx, |root| {
            init_repo(root);
            let hook = root.join(".git/hooks/pre-commit");
            std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("the hook is written");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("the hook runs");
            std::fs::write(root.join("a.txt"), "one changed\n").expect("the file is written");
        });
        run_action(cx, "vcs.commit", None);
        let (panel, commit_box) = commit_parts(&workbench, cx);
        wait_long(cx, "the change is listed", |cx| panel.read_with(cx, |panel, _| panel.lines().len()) == 2);
        wait_long(cx, "the box counts it", |cx| commit_box.read_with(cx, |b, _| b.input().included == 1));
        cx.simulate_input("Refused");
        click_on(cx, "commit-commit");
        wait_long(cx, "the failure is reported", |cx| {
            notification_titles(&workbench, cx).contains(&"Commit failed".to_owned())
        });
        assert_eq!(git_in(&root, &["rev-list", "--count", "HEAD"]), "1", "nothing was committed");
        assert_eq!(commit_message(&commit_box, cx), "Refused", "the message stays");
        assert!(commit_box.read_with(cx, |commit_box, _| !commit_box.input().busy));

        click_on(cx, "commit-options");
        assert!(commit_box.read_with(cx, |commit_box, _| commit_box.options_open()), "the options open");
        // Analyze code is on by default: the click turns it off.
        click_on(cx, "commit-check-analyze");
        let analyze = cx.update(|_, cx| crate::platform::settings::effective("w1", cx).vcs.commit.analyze_code);
        assert!(!analyze, "the check is written to the settings");
        click_on(cx, "commit-run-hooks");
        cx.simulate_keystrokes("escape");
        run_action(cx, "vcs.commit", None);
        cx.simulate_keystrokes(COMMIT_KEYS);
        wait_long(cx, "the commit without hooks", |cx| {
            notification_titles(&workbench, cx).contains(&"1 file committed: Refused".to_owned())
        });
        assert_eq!(git_in(&root, &["log", "-1", "--format=%s"]), "Refused");
    }

    /// A diagnostic as a producer publishes it.
    pub(super) fn published(line: u32, col: u32, severity: ide_core::Severity, message: &str) -> ide_core::Diagnostic {
        ide_core::Diagnostic {
            range: ide_core::Range::new(ide_core::Pos::new(line, col), ide_core::Pos::new(line, col + 3)),
            severity,
            message: message.to_owned(),
            source: Some("checker".to_owned()),
            code: None,
            unnecessary: false,
            deprecated: false,
            server: None,
        }
    }

    /// Hands the window the diagnostics of `path` the way the backend sends them.
    pub(super) fn publish(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
        path: &std::path::Path,
        items: Vec<ide_core::Diagnostic>,
    ) {
        let event = BackendEvent {
            window: Some("w1".to_owned()),
            event: IdeEvent::Diagnostics {
                path: ide_core::IpcPath::from(path.to_path_buf()),
                source: "checker".to_owned(),
                items,
            },
        };
        workbench.update(cx, |workbench, cx| workbench.backend_event(&event, cx));
        cx.run_until_parked();
    }

    /// The Problems tool window lists the problems the backend publishes by file, with the counts
    /// of its tabs; Enter on a problem opens its file at the problem, and a file whose diagnostics
    /// are cleared leaves the list.
    #[gpui_kit::test]
    async fn the_problems_window_lists_published_problems_and_opens_the_chosen_one(cx: &mut TestAppContext) {
        use crate::code_insight::problems::ProblemsTab;
        use crate::platform::i18n::Localization;
        use ide_core::Severity;
        let _home = start(cx);
        let (_folder, root, workbench, cx) =
            trusted_project(cx, &[("src/a.txt", "one\ntwo\nthree\n"), ("src/b.txt", "alpha\nbeta\n")]);
        cx.update(|window, _| window.activate_window());
        let (a, b) = (root.join("src/a.txt"), root.join("src/b.txt"));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(a.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));

        run_action(cx, "toolwindow.problems", None);
        let view = workbench.read_with(cx, |w, _| w.problems_view()).expect("the Problems window is shown");
        publish(
            &workbench,
            cx,
            &a,
            vec![published(2, 0, Severity::Error, "second error"), published(0, 1, Severity::Error, "first error")],
        );
        publish(&workbench, cx, &b, vec![published(1, 2, Severity::Error, "beta error")]);
        view.update(cx, |view, cx| view.select_tab(ProblemsTab::Project, cx));
        assert_eq!(
            view.read_with(cx, |view, _| view.lines()),
            ["a.txt", "  first error :1", "  second error :3", "b.txt", "  beta error :2"]
        );
        // The File tab counts the editor's file, Project Errors the whole project.
        let counts = view
            .update(cx, |view, cx| view.tab_labels(&cx.default_global::<Localization>().0).map(|(_, _, count)| count));
        assert_eq!(counts, [Some(2), Some(3)]);
        assert_eq!(workbench.read_with(cx, |w, _| w.problems_badge()), Some(3));

        // Enter on the last problem opens its file with the caret on it.
        cx.update(|window, cx| {
            let handle = view.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("down down down down down enter");
        let at = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, cx| {
                w.editor().map(|e| (e.read(cx).path().to_path_buf(), e.read(cx).caret_position()))
            })
        };
        settle(cx, |cx| at(cx) == Some((b.clone(), (1, 2))));

        // A file whose diagnostics are cleared leaves the list.
        publish(&workbench, cx, &b, Vec::new());
        assert_eq!(view.read_with(cx, |view, _| view.lines()), ["a.txt", "  first error :1", "  second error :3"]);
        assert_eq!(workbench.read_with(cx, |w, _| w.problems_badge()), Some(2));
    }

    /// Without a language server Find Usages lists the text matches of the name at the caret by
    /// file and says they are text matches; Backspace excludes a usage and Enter opens one. Show
    /// Usages lists the same usages in its popup and opens the one Enter is pressed on.
    #[gpui_kit::test]
    async fn find_usages_without_a_server_lists_text_matches_by_file_and_show_usages_opens_the_chosen_one(
        cx: &mut TestAppContext,
    ) {
        use crate::code_insight::usages::usage_key;
        use crate::code_insight::usages_view::RowKey;
        use crate::platform::i18n::Localization;
        let _home = start(cx);
        let (_folder, root, workbench, cx) =
            trusted_project(cx, &[("notes/a.txt", "shared value\n"), ("notes/b.txt", "use shared\nshared again\n")]);
        cx.update(|window, _| window.activate_window());
        let (a, b) = (root.join("notes/a.txt"), root.join("notes/b.txt"));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(a.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));

        run_action(cx, "find.usages", None);
        let view = workbench.read_with(cx, |w, _| w.usages_view()).expect("the Find Usages window is shown");
        settle(cx, |cx| view.read_with(cx, |view, _| view.search().is_some_and(|search| !search.loading)));
        let rows = |cx: &mut VisualTestContext| {
            view.update(cx, |view, cx| {
                let rows = view.rows(&cx.default_global::<Localization>().0);
                rows.into_iter().map(|row| row.text).collect::<Vec<_>>()
            })
        };
        let light_mode = workbench.read_with(cx, |w, cx| !w.ide_mode(cx).is_full());
        let note = cx.update(|_, cx| {
            crate::editor_lsp::lsp::approximate_note(&cx.default_global::<Localization>().0, light_mode)
        });
        assert_eq!(
            rows(cx),
            [
                format!("Found usages 3 results {note}"),
                "a.txt  notes  1 usage".to_owned(),
                "1 shared value".to_owned(),
                "b.txt  notes  2 usages".to_owned(),
                "1 use shared".to_owned(),
                "2 shared again".to_owned(),
            ]
        );

        // Backspace excludes the selected usage.
        let usages: Vec<crate::code_insight::usages::Usage> = view.read_with(cx, |view, _| {
            let groups = view.search().map(|search| search.groups.as_slice()).unwrap_or_default();
            groups.iter().flat_map(|group| group.usages.clone()).collect()
        });
        let key = |at: usize| RowKey::Usage(usage_key(&usages[at]));
        view.update(cx, |view, cx| view.select_row(key(2), cx));
        cx.update(|window, cx| {
            let handle = view.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("backspace");
        assert!(view.read_with(cx, |view, _| view.is_excluded(&key(2))));
        assert!(!view.read_with(cx, |view, _| view.is_excluded(&key(1))));

        // Enter opens the selected usage with the caret on it.
        view.update(cx, |view, cx| view.select_row(key(1), cx));
        cx.simulate_keystrokes("enter");
        let at = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, cx| {
                w.editor().map(|e| (e.read(cx).path().to_path_buf(), e.read(cx).caret_position()))
            })
        };
        settle(cx, |cx| at(cx) == Some((b.clone(), (0, 4))));
        // Show Usages' `when` wants the keyboard in the editor, not in the tool window.
        settle(cx, |cx| {
            cx.update(|window, cx| {
                workbench.read(cx).editor().is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
            })
        });

        // Show Usages on the same name lists the same usages; Enter opens the chosen one.
        run_action(cx, "find.showUsages", None);
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| {
                w.usages_popup().is_some_and(|popup| {
                    let popup = popup.read(cx);
                    popup.rows().len() == 3 && !popup.is_searching()
                })
            })
        });
        let popup = workbench.read_with(cx, |w, _| w.usages_popup()).expect("the popup is open");
        let listed = popup.read_with(cx, |popup, _| {
            popup.rows().iter().map(|usage| (usage.path.clone(), usage.line, usage.col)).collect::<Vec<_>>()
        });
        let path = |file: &std::path::Path| crate::platform::paths::ipc_path(&file.to_string_lossy());
        assert_eq!(listed, [(path(&a), 0, 0), (path(&b), 0, 4), (path(&b), 1, 0)]);
        cx.simulate_keystrokes("down down enter");
        settle(cx, |cx| at(cx) == Some((b.clone(), (1, 0))));
        assert!(workbench.read_with(cx, |w, _| w.usages_popup().is_none()));
    }

    /// Without a language server Type Hierarchy shows the Hierarchy tool window for the name at the
    /// caret and says that no language server is available.
    #[gpui_kit::test]
    async fn type_hierarchy_without_a_server_says_no_language_server_is_available(cx: &mut TestAppContext) {
        use crate::code_insight::hierarchy::ROOT_ID;
        use crate::editor_lsp::hint::Tone;
        use crate::platform::i18n::Localization;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("notes/a.txt", "shared value\n")]);
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes/a.txt"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));

        run_action(cx, "navigate.typeHierarchy", None);
        let view = workbench.read_with(cx, |w, _| w.hierarchy_view()).expect("the Hierarchy window is shown");
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.code_insight_hint().is_some()));
        let hint = workbench.read_with(cx, |w, _| w.code_insight_hint().cloned()).expect("a hint");
        assert_eq!((hint.text.as_str(), hint.tone), (crate::editor_lsp::lsp::NO_SERVER, Tone::Info));
        workbench.read_with(cx, |w, _| {
            assert_eq!(w.hierarchy().root().map(|root| root.name.as_str()), Some("shared"));
            assert!(w.hierarchy().failed(ROOT_ID));
            assert!(!w.hierarchy().loading());
        });
        let title = view.update(cx, |view, cx| view.title(&mut cx.default_global::<Localization>().0));
        assert_eq!(title.as_deref(), Some("Subtypes of shared"));
    }

    /// The setting `key` of `settings`, as the settings file writes it.
    fn setting_of(settings: &ide_core::settings::Settings, key: &str) -> Option<serde_json::Value> {
        use crate::settings_model::schema::{get_at, settings_value};
        get_at(&settings_value(settings), key).cloned()
    }

    /// The window's Settings dialog, while it is open.
    fn settings_dialog_of(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<crate::settings_view::dialog::SettingsDialogView>> {
        workbench.read_with(cx, |w, _| w.settings_dialog().cloned())
    }

    /// The text of the global settings file under `home`; empty while there is none.
    fn global_settings_file(home: &tempfile::TempDir) -> String {
        std::fs::read_to_string(home.path().join("config/settings.toml")).unwrap_or_default()
    }

    /// File › Settings… opens on the first page with the keyboard in the search. A search narrows
    /// the tree to the pages that have its words, lands on the first of them and marks the option
    /// that has them. A checkbox ticked there enables Apply. Apply writes the setting to the global
    /// settings file and keeps the dialog open, with nothing left to apply. OK then closes it.
    #[gpui_kit::test]
    async fn settings_searches_the_pages_applies_a_ticked_checkbox_and_ok_closes(cx: &mut TestAppContext) {
        const KEY: &str = "editor.general.scrollPastEnd";
        let home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        run_action(cx, "file.settings", None);
        let open = settings_dialog_of(&workbench, cx).expect("Settings opens");
        assert_eq!(open.read_with(cx, |view, cx| view.selected_page(cx)).as_deref(), Some("appearance"));
        assert!(!open.read_with(cx, |view, cx| view.can_apply(cx)));
        let in_search = cx.update(|window, cx| open.read(cx).search_field().focus_handle(cx).is_focused(window));
        assert!(in_search, "the keyboard is in the search");

        cx.simulate_input("virtual space");
        cx.run_until_parked();
        assert_eq!(open.read_with(cx, |view, cx| view.selected_page(cx)).as_deref(), Some("editor.general"));
        let pages = open.read_with(cx, |view, cx| view.pages(cx));
        assert!(pages.contains(&"  General".to_owned()), "{pages:?}");
        assert!(!pages.iter().any(|page| page.trim() == "Keymap"), "{pages:?}");
        let marked = open.read_with(cx, |view, cx| view.highlighted(cx));
        assert!(marked.contains(&"Show virtual space at file bottom".to_owned()), "{marked:?}");

        click_on(cx, "settings-toggle-editor.general.scrollPastEnd");
        assert!(open.read_with(cx, |view, cx| view.can_apply(cx)), "an edit enables Apply");
        click_on(cx, "settings-apply");
        let effective = cx.update(|_, cx| setting_of(&crate::platform::settings::effective("w1", cx), KEY));
        assert_eq!(effective, Some(serde_json::json!(true)));
        let stored = global_settings_file(&home);
        assert!(stored.contains("scrollPastEnd = true"), "{stored}");
        assert!(settings_dialog_of(&workbench, cx).is_some(), "Apply keeps the dialog open");
        assert!(!open.read_with(cx, |view, cx| view.can_apply(cx)), "nothing is left to apply");

        click_on(cx, "settings-ok");
        assert!(settings_dialog_of(&workbench, cx).is_none(), "OK closes the dialog");
    }

    /// Cancel after ticking a checkbox closes the dialog and writes nothing: the setting keeps its
    /// value and the settings file does not name it. Opened again, the dialog has nothing to apply,
    /// and Escape in its empty search closes it.
    #[gpui_kit::test]
    async fn settings_cancel_after_an_edit_writes_nothing(cx: &mut TestAppContext) {
        const KEY: &str = "editor.general.wheelZoom";
        let home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        run_action(cx, "file.settings", Some(serde_json::Value::from("editor.general")));
        let open = settings_dialog_of(&workbench, cx).expect("Settings opens");
        assert_eq!(open.read_with(cx, |view, cx| view.selected_page(cx)).as_deref(), Some("editor.general"));
        click_on(cx, "settings-toggle-editor.general.wheelZoom");
        assert!(open.read_with(cx, |view, cx| view.can_apply(cx)));
        click_on(cx, "settings-cancel");
        assert!(settings_dialog_of(&workbench, cx).is_none(), "Cancel closes the dialog");
        let effective = cx.update(|_, cx| setting_of(&crate::platform::settings::effective("w1", cx), KEY));
        assert_eq!(effective, Some(serde_json::json!(false)));
        let stored = global_settings_file(&home);
        assert!(!stored.contains("wheelZoom"), "{stored}");

        run_action(cx, "file.settings", Some(serde_json::Value::from("editor.general")));
        let open = settings_dialog_of(&workbench, cx).expect("Settings opens again");
        assert!(!open.read_with(cx, |view, cx| view.can_apply(cx)), "the edit was discarded");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(settings_dialog_of(&workbench, cx).is_none(), "Escape in the empty search closes it");
    }

    /// With a project open, General (a page a project can override) writes to the project's settings
    /// file under This project and leaves the global layer alone. Under Global it shows and writes
    /// the global layer.
    #[gpui_kit::test]
    async fn settings_write_the_project_layer_under_this_project_and_the_global_one_under_global(
        cx: &mut TestAppContext,
    ) {
        use crate::settings_model::staging::Scope;
        const KEY: &str = "editor.general.wheelZoom";
        let home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("a.txt", "a\n")]);
        cx.update(|window, _| window.activate_window());
        run_action(cx, "file.settings", Some(serde_json::Value::from("editor.general")));
        let open = settings_dialog_of(&workbench, cx).expect("Settings opens");
        assert!(open.read_with(cx, |view, cx| view.has_scope_switch(cx)), "General offers This project");
        assert_eq!(open.read_with(cx, |view, cx| view.scope(cx)), Scope::Global);

        click_on(cx, "settings-scope-project");
        assert_eq!(open.read_with(cx, |view, cx| view.scope(cx)), Scope::Project);
        click_on(cx, "settings-toggle-editor.general.wheelZoom");
        click_on(cx, "settings-apply");
        let project_file = std::fs::read_to_string(root.join(".openidea/settings.toml")).unwrap_or_default();
        assert!(project_file.contains("wheelZoom = true"), "{project_file}");
        let effective = cx.update(|_, cx| setting_of(&crate::platform::settings::effective("w1", cx), KEY));
        assert_eq!(effective, Some(serde_json::json!(true)));
        let global = cx.update(|_, cx| setting_of(&crate::platform::settings::global(cx), KEY));
        assert_eq!(global, Some(serde_json::json!(false)), "the global layer is left alone");
        assert!(!global_settings_file(&home).contains("wheelZoom"));

        click_on(cx, "settings-scope-global");
        let lines = open.read_with(cx, |view, cx| view.control_lines(cx));
        assert!(lines.contains(&"[ ] Change font size with Command+Mouse Wheel".to_owned()), "{lines:?}");
        click_on(cx, "settings-toggle-editor.general.wheelZoom");
        click_on(cx, "settings-apply");
        let global = cx.update(|_, cx| setting_of(&crate::platform::settings::global(cx), KEY));
        assert_eq!(global, Some(serde_json::json!(true)));
        let stored = global_settings_file(&home);
        assert!(stored.contains("wheelZoom = true"), "{stored}");
        click_on(cx, "settings-ok");
        assert!(settings_dialog_of(&workbench, cx).is_none());
    }

    /// Keymap… opens Settings on the Keymap page. A shortcut recorded for an action in its Keyboard
    /// Shortcut dialog and applied is stored in `keymapOverrides`, and the keymap resolves it to the
    /// action from then on.
    #[gpui_kit::test]
    async fn settings_keymap_assigns_a_recorded_shortcut_that_the_keymap_then_resolves(cx: &mut TestAppContext) {
        const ACTION: &str = "navigate.gotoLine";
        let home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        run_action(cx, "settings.keymap", None);
        let open = settings_dialog_of(&workbench, cx).expect("Settings opens");
        assert_eq!(open.read_with(cx, |view, cx| view.selected_page(cx)).as_deref(), Some("keymap"));
        let keymap = workbench.read_with(cx, |w, _| w.settings_keymap().cloned()).expect("Keymap has its view");

        keymap.update_in(cx, |view, window, cx| view.start_recording(ACTION, false, window, cx));
        cx.run_until_parked();
        let recording = keymap.read_with(cx, |view, _| view.recording().map(|(id, mouse)| (id.to_owned(), mouse)));
        assert_eq!(recording, Some((ACTION.to_owned(), false)), "the Keyboard Shortcut dialog is open");
        cx.simulate_keystrokes("alt-shift-f11");
        let binding = keymap.read_with(cx, |view, _| view.recorded_binding()).expect("the stroke is recorded");
        keymap.update_in(cx, |view, window, cx| view.confirm_recording(window, cx));
        if keymap.read_with(cx, |view, _| view.asks_to_remove_conflicts()) {
            // Leave: the other assignments keep the stroke; this one wins as the later binding.
            keymap.update_in(cx, |view, window, cx| view.submit_recording(false, window, cx));
        }
        cx.run_until_parked();
        assert!(keymap.read_with(cx, |view, _| view.recording().is_none()), "the dialog closed");
        assert!(open.read_with(cx, |view, cx| view.can_apply(cx)), "the shortcut is staged");

        click_on(cx, "settings-apply");
        let stored = cx.update(|_, cx| crate::platform::settings::global(cx).keymap_overrides.get(ACTION).cloned());
        assert!(stored.is_some_and(|keys| keys.contains(&binding)), "keymapOverrides holds the shortcut");
        let file = global_settings_file(&home);
        assert!(file.contains("keymapOverrides") && file.contains(ACTION), "{file}");
        settle(cx, |cx| cx.update(|_, cx| cx.global::<input::Keymap>().0.bindings_for(ACTION).contains(&binding)));
        let resolved = cx.update(|_, cx| {
            let effective = cx.global::<input::Keymap>().0.effective_bindings();
            effective.iter().rev().find(|(key, _)| *key == binding).map(|(_, id)| id.clone())
        });
        assert_eq!(resolved.as_deref(), Some(ACTION), "the shortcut runs the action");
    }

    /// Appearance… opens Settings on Appearance. The theme chosen there and applied is the one the
    /// settings name and the one the windows are drawn with.
    #[gpui_kit::test]
    async fn settings_appearance_applies_the_chosen_theme(cx: &mut TestAppContext) {
        let _home = start(cx);
        cx.update(crate::theme::appearance::init);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        run_action(cx, "settings.appearance", None);
        let open = settings_dialog_of(&workbench, cx).expect("Settings opens");
        assert_eq!(open.read_with(cx, |view, cx| view.selected_page(cx)).as_deref(), Some("appearance"));
        assert!(!cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));

        click_on(cx, "settings-select-appearance.theme");
        click_on(cx, "settings-choice-appearance.theme-dark");
        let lines = open.read_with(cx, |view, cx| view.control_lines(cx));
        assert!(lines.contains(&"Theme: Islands Dark ▾".to_owned()), "{lines:?}");
        click_on(cx, "settings-apply");
        assert_eq!(cx.update(|_, cx| crate::platform::settings::global(cx).appearance.theme), "dark");
        settle(cx, |cx| cx.update(|_, cx| cx.global::<theme::ActiveTheme>().is_dark()));
    }

    /// Every code insight action runs in a window with a project: Call Hierarchy, Find Usages and
    /// its Rerun, a list of usages handed to Show Usage Results, and Show Usages closed with Escape.
    /// Language server reports, a settings change and closing the project with the three tool
    /// windows shown are taken without fault.
    #[gpui_kit::test]
    async fn the_code_insight_actions_and_events_run_in_a_window_with_a_project(cx: &mut TestAppContext) {
        use crate::platform::i18n::Localization;
        use ide_core::event::ServerStatus;
        let _home = start(cx);
        let (_folder, root, workbench, cx) =
            trusted_project(cx, &[("notes/a.txt", "shared value\n"), ("notes/b.txt", "use shared\n")]);
        cx.update(|window, _| window.activate_window());
        let a = root.join("notes/a.txt");
        workbench.update_in(cx, |w, window, cx| w.open_project_file(a.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));

        run_action(cx, "navigate.callHierarchy", None);
        settle(cx, |cx| workbench.read_with(cx, |w, _| !w.hierarchy().loading()));
        assert!(workbench.read_with(cx, |w, _| w.hierarchy_view()).is_some(), "the Hierarchy window is shown");
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        cx.run_until_parked();

        run_action(cx, "find.usages", None);
        let view = workbench.read_with(cx, |w, _| w.usages_view()).expect("the Find Usages window is shown");
        let done = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| view.search().is_some_and(|search| !search.loading))
        };
        settle(cx, done);
        let rows = |cx: &mut VisualTestContext| {
            view.update(cx, |view, cx| {
                view.rows(&cx.default_global::<Localization>().0).into_iter().map(|row| row.text).collect::<Vec<_>>()
            })
        };
        let found = rows(cx);
        assert!(found.len() > 1, "{found:?}");
        // Rerun is an action of the Find Usages window: it wants the keyboard there.
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.run_until_parked();
        run_action(cx, "usages.rerun", None);
        settle(cx, done);
        assert_eq!(rows(cx), found, "Rerun finds the same usages");

        let location = ide_core::Location {
            path: crate::platform::paths::ipc_path(&root.join("notes/b.txt").to_string_lossy()).into(),
            range: ide_core::Range::new(ide_core::Pos::new(0, 4), ide_core::Pos::new(0, 10)),
            uri: None,
        };
        let arg = serde_json::json!({ "symbol": "shared", "locations": [location] });
        run_action(cx, "find.showUsageResults", Some(arg));
        // The usage's line is read from its file before the list is drawn.
        settle(cx, |cx| rows(cx).iter().any(|row| row.contains("b.txt")));

        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        cx.run_until_parked();
        run_action(cx, "find.showUsages", None);
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.usages_popup().is_some()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.usages_popup().is_none()), "Escape closes Show Usages");

        // The item lists a server as soon as its event arrives. The snapshot that event asks for
        // comes from the backend, which runs no server here, so the list is read before it lands.
        let send = |event: IdeEvent, cx: &mut VisualTestContext| {
            let event = BackendEvent { window: Some("w1".to_owned()), event };
            workbench.update(cx, |workbench, cx| workbench.backend_event(&event, cx));
        };
        let server = ide_core::ServerId(7);
        let name = "checker".to_owned();
        send(IdeEvent::LanguageServer { server, name, status: ServerStatus::Ready, message: None }, cx);
        let listed = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, cx| w.server_status_item().read(cx).servers().len())
        };
        assert_eq!(listed(cx), 1);
        send(IdeEvent::LanguageServerRemoved { server }, cx);
        assert_eq!(listed(cx), 0);
        cx.run_until_parked();
        assert_eq!(listed(cx), 0, "a removed server stays removed");
        send(IdeEvent::SettingsChanged { key: "ide.mode".to_owned() }, cx);
        cx.run_until_parked();

        run_action(cx, "toolwindow.problems", None);
        assert!(workbench.read_with(cx, |w, _| w.problems_view()).is_some());
        workbench.update_in(cx, |w, window, cx| w.close_project(window, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.usages_view().is_none() && w.problems_view().is_none()));
    }

    /// Settings for New Projects opens on the Java page and offers no This project scope, even with a
    /// project open; Plugins… opens Settings on the Plugins page, which says it is not available in a
    /// window without a plugin host.
    #[gpui_kit::test]
    async fn settings_for_new_projects_and_plugins_open_on_their_pages(cx: &mut TestAppContext) {
        use crate::settings_model::staging::Scope;
        let _home = start(cx);
        let (_folder, _root, workbench, cx) = trusted_project(cx, &[("a.txt", "a\n")]);
        cx.update(|window, _| window.activate_window());
        run_action(cx, "file.newProjectsSettings", None);
        let open = settings_dialog_of(&workbench, cx).expect("Settings for New Projects opens");
        assert_eq!(open.read_with(cx, |view, cx| view.selected_page(cx)).as_deref(), Some("languages.java"));
        assert!(!open.read_with(cx, |view, cx| view.has_scope_switch(cx)), "new projects edit the global layer");
        assert_eq!(open.read_with(cx, |view, cx| view.scope(cx)), Scope::Global);
        click_on(cx, "settings-cancel");
        assert!(settings_dialog_of(&workbench, cx).is_none());

        run_action(cx, "plugins.settings", None);
        let open = settings_dialog_of(&workbench, cx).expect("Settings opens");
        assert_eq!(open.read_with(cx, |view, cx| view.selected_page(cx)).as_deref(), Some("plugins"));
        assert!(workbench.read_with(cx, |w, _| w.settings_plugins_page().is_none()), "no plugin host here");
        let lines = open.read_with(cx, |view, cx| view.control_lines(cx));
        assert_eq!(lines, [crate::settings_view::controls::UNAVAILABLE]);
    }

    /// A 2×2 red PNG.
    const PNG_2X2: [u8; 74] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xB6, 0x0D, 0x24, 0x00, 0x00, 0x00,
        0x11, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0x84, 0x19, 0x60, 0x0C, 0x00,
        0x47, 0xCA, 0x07, 0xF9, 0x1A, 0xB6, 0xF1, 0xA9, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42,
        0x60, 0x82,
    ];

    fn context_is_on(cx: &mut VisualTestContext, key: &str) -> bool {
        cx.update(|window, cx| crate::platform::context::window_context(window, cx).is_truthy(key))
    }

    /// The text a context key holds, while it holds one.
    fn context_text(cx: &mut VisualTestContext, key: &str) -> Option<String> {
        cx.update(|window, cx| match crate::platform::context::window_context(window, cx).get(key) {
            Some(crate::platform::context::ContextValue::Text(text)) => Some(text.to_string()),
            _ => None,
        })
    }

    /// The window's context keys follow what it shows and where the keyboard is, each read on and
    /// off: `projectOpening` and `projectOpen` (no project, opening, open, closed); `editorFocus`,
    /// `editorTextFocus`, `editorAreaFocus` and `editorPath` (the editor has the keyboard, then the
    /// Project view, then no tab is open); `activeToolWindow` (the Project view, the Terminal, the
    /// editor); `terminalFocus`; `vcsAvailable` (a folder without a repository, then one with).
    #[gpui_kit::test]
    async fn context_keys_follow_the_project_the_editor_the_tool_windows_and_the_repository(cx: &mut TestAppContext) {
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let plain = tempfile::tempdir().expect("a temporary folder");
        let root = plain.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("notes.txt"), "notes\n").expect("the file is written");
        let versioned = tempfile::tempdir().expect("a temporary folder");
        let repo = versioned.path().canonicalize().expect("the folder exists");
        init_repo(&repo);
        for folder in [&root, &repo] {
            cx.update(|cx| cx.global::<Backend>().host.trust.decide(folder, true, false)).expect("trusted");
        }
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx))
                .unwrap_or_else(|e| panic!("{id}: {e}"));
            cx.run_until_parked();
        };
        let editor_keys = ["editorFocus", "editorTextFocus", "editorAreaFocus"];

        // No project.
        cx.run_until_parked();
        assert!(!context_is_on(cx, "projectOpen") && !context_is_on(cx, "projectOpening"));
        assert!(editor_keys.iter().all(|key| !context_is_on(cx, key)));
        assert_eq!(context_text(cx, "editorPath"), None);

        // Opening, then open.
        workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
        assert!(context_is_on(cx, "projectOpening"), "the project is opening");
        assert!(!context_is_on(cx, "projectOpen"));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        assert!(context_is_on(cx, "projectOpen") && !context_is_on(cx, "projectOpening"));
        assert!(!context_is_on(cx, "vcsAvailable"), "the folder has no repository");

        // The editor has the keyboard.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx, |cx| {
            let editor = workbench.read_with(cx, |w, _| w.editor().cloned());
            cx.update(|window, cx| editor.is_some_and(|editor| editor.focus_handle(cx).is_focused(window)))
        });
        assert!(editor_keys.iter().all(|key| context_is_on(cx, key)));
        let path = root.join("notes.txt").to_string_lossy().into_owned();
        assert_eq!(context_text(cx, "editorPath").as_deref(), Some(path.as_str()));
        assert_eq!(context_text(cx, "activeToolWindow"), None, "no tool window has the keyboard");

        // The Project view has it.
        run(cx, "toolwindow.project");
        assert!(editor_keys.iter().all(|key| !context_is_on(cx, key)));
        assert_eq!(context_text(cx, "activeToolWindow").as_deref(), Some("project"));
        assert!(!context_is_on(cx, "terminalFocus"));

        // The Terminal has it, then the editor again.
        run(cx, "toolwindow.terminal");
        assert!(context_is_on(cx, "terminalFocus"));
        assert_eq!(context_text(cx, "activeToolWindow").as_deref(), Some("terminal"));
        cx.update(|window, cx| {
            let editor = workbench.read(cx).editor().cloned().expect("an editor");
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        assert!(!context_is_on(cx, "terminalFocus"));
        assert!(editor_keys.iter().all(|key| context_is_on(cx, key)));
        assert_eq!(context_text(cx, "activeToolWindow"), None);

        // No tab: no editor path.
        run(cx, "editor.closeAllTabs");
        assert_eq!(context_text(cx, "editorPath"), None);
        assert!(editor_keys.iter().all(|key| !context_is_on(cx, key)));

        // Closed, then a project with a repository.
        run(cx, "file.closeProject");
        assert!(!context_is_on(cx, "projectOpen") && !context_is_on(cx, "vcsAvailable"));
        workbench.update_in(cx, |w, window, cx| w.open_project(repo.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        wait_long(cx, "the repository is read", |cx| context_is_on(cx, "vcsAvailable"));
        assert!(context_is_on(cx, "projectOpen"));
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    #[gpui_kit::test]
    async fn a_png_opened_from_the_project_view_shows_in_an_image_tab_that_zooms_and_closes_without_asking(
        cx: &mut TestAppContext,
    ) {
        use crate::features_b::viewers::image_view::ImageView;
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("logo.png"), PNG_2X2).expect("the image is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let tree = workbench.read_with(cx, |w, _| w.project_tree().cloned()).expect("a tree");
        settle(cx, |cx| tree.read_with(cx, |tree, _| tree.rows().len() == 2));
        cx.simulate_keystrokes("down down enter");
        let image = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.active_tab_view().and_then(|view| view.downcast::<ImageView>()))
        };
        // Shown once decoded and placed in its canvas.
        settle(cx, |cx| image(cx).is_some_and(|view| view.read_with(cx, |view, _| view.view().is_some())));
        workbench.read_with(cx, |w, _| {
            assert_eq!(w.tab_titles().into_iter().map(|tab| tab.name).collect::<Vec<_>>(), ["logo.png"]);
            assert!(w.editor().is_none(), "an image tab has no text editor");
        });
        cx.run_until_parked();
        assert!(!context_is_on(cx, "textEditorActive"));
        assert!(context_is_on(cx, "imageViewerActive"));
        assert!(context_is_on(cx, "imageViewerFocus"), "the image has the keyboard");

        let label = |cx: &mut VisualTestContext| image(cx).expect("the image tab").read_with(cx, |view, _| view.info());
        let before = label(cx);
        assert!(before.starts_with("2×2 · ") && before.ends_with('%'), "{before}");
        cx.update(|window, cx| execute_action_or_throw("image.zoomIn", None, window, cx)).expect("it runs");
        cx.run_until_parked();
        assert_ne!(label(cx), before);
        // The Project view takes the keyboard: the image tab stays active without it.
        cx.update(|window, cx| execute_action_or_throw("toolwindow.project", None, window, cx)).expect("it runs");
        cx.run_until_parked();
        assert!(context_is_on(cx, "imageViewerActive") && !context_is_on(cx, "imageViewerFocus"));
        // Escape in the tool window gives the keyboard back to the image.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(context_is_on(cx, "imageViewerFocus"));

        // An image holds no unsaved changes: closing its tab asks nothing.
        cx.update(|window, cx| execute_action_or_throw("editor.closeTab", None, window, cx)).expect("it runs");
        cx.run_until_parked();
        assert!(cx.pending_prompt().is_none());
        assert!(workbench.read_with(cx, |w, _| w.tab_titles().is_empty() && w.active_tab_view().is_none()));
        assert!(!context_is_on(cx, "imageViewerActive"));
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn a_file_with_nul_bytes_opens_in_the_hex_view_and_open_as_text_gives_it_a_text_editor(
        cx: &mut TestAppContext,
    ) {
        use crate::features_b::viewers::hex_view::HexView;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        let data = root.join("data.bin");
        std::fs::write(&data, b"\0\0binary payload").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(data.clone(), window, cx));
        let hex = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.active_tab_view().and_then(|view| view.downcast::<HexView>()))
        };
        let first_row = |cx: &mut VisualTestContext| {
            hex(cx).and_then(|view| view.read_with(cx, |view, _| view.rows().into_iter().next()))
        };
        settle(cx, |cx| first_row(cx).is_some_and(|row| !row.contains("··")));
        assert_eq!(
            first_row(cx).as_deref(),
            Some("00000000  00 00 62 69 6E 61 72 79  20 70 61 79 6C 6F 61 64  ..binary payload")
        );
        workbench.read_with(cx, |w, _| {
            assert_eq!(w.tab_titles().into_iter().map(|tab| tab.name).collect::<Vec<_>>(), ["data.bin"]);
            assert!(w.editor().is_none());
        });
        // The probe decided before any document was opened.
        let host = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        assert!(host.project("w1").is_ok_and(|project| project.documents.open_documents().is_empty()));
        cx.run_until_parked();
        assert!(!context_is_on(cx, "textEditorActive"));

        // Open as Text gives the file a text editor, read-only for binary content.
        workbench.update_in(cx, |w, window, cx| w.open_project_file_as_text(data.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == data)));
        cx.run_until_parked();
        assert!(context_is_on(cx, "textEditorActive") && context_is_on(cx, "editorReadOnly"));
        assert!(workbench.read_with(cx, |w, _| w.active_tab_view().is_none()));
        // Opened again, the file goes to its text tab, past the viewers.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(data.clone(), window, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == data)));
        host.close_project("w1");
    }

    #[gpui_kit::test]
    async fn a_markdown_file_opens_as_text_with_its_layouts_and_a_preview_that_follows_the_typing(
        cx: &mut TestAppContext,
    ) {
        use crate::features_b::viewers::preview::{MarkdownMode, TEXT_DELAY_MS};
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        let readme = root.join("README.md");
        std::fs::write(&readme, "# Title\n\n- first item\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        workbench.update_in(cx, |w, window, cx| w.open_project_file(readme.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.editor().is_some() && w.active_markdown_host().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("a text editor");
        let host = workbench.read_with(cx, |w, _| w.active_markdown_host()).expect("the Markdown layouts");
        cx.run_until_parked();
        assert!(context_is_on(cx, "textEditorActive"));
        let language =
            cx.update(|window, cx| crate::platform::context::window_context(window, cx).get("editorLangId").cloned());
        assert_eq!(language, Some(crate::platform::context::ContextValue::from("markdown")));
        // The layout buttons are in the tab strip, not floating over the editor.
        assert!(cx.debug_bounds("markdown-layout-split").is_some());
        assert!(cx.debug_bounds("markdown-layout-actions").is_none());

        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        run(cx, "markdown.editorOnly");
        assert!(host.read_with(cx, |host, _| host.preview().is_none()));
        run(cx, "markdown.editorAndPreview");
        let preview = host.read_with(cx, |host, _| host.preview().cloned()).expect("the preview shows");
        let blocks = |cx: &mut VisualTestContext| -> Vec<(String, String)> {
            preview.read_with(cx, |preview, _| {
                preview.blocks().iter().flat_map(|block| &block.nodes).map(|node| (node.tag(), node.text())).collect()
            })
        };
        let shown = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(tag, text)| ((*tag).to_owned(), (*text).to_owned())).collect()
        };
        assert_eq!(blocks(cx), shown(&[("H1", "Title"), ("UL", "first item")]));

        // What is typed reaches the preview once typing pauses.
        cx.simulate_keystrokes("right right");
        cx.simulate_input("My ");
        assert_eq!(editor.read_with(cx, |e, _| e.text()), "# My Title\n\n- first item\n");
        assert_eq!(blocks(cx), shown(&[("H1", "Title"), ("UL", "first item")]));
        cx.executor().advance_clock(Duration::from_millis(TEXT_DELAY_MS));
        cx.run_until_parked();
        assert_eq!(blocks(cx), shown(&[("H1", "My Title"), ("UL", "first item")]));

        // Preview Only hides the editor; the tab stays, and stays a text tab.
        run(cx, "markdown.previewOnly");
        assert_eq!(host.read_with(cx, |host, _| host.mode()), MarkdownMode::Preview);
        assert!(cx.debug_bounds("markdown-editor-slot").is_none());
        assert!(cx.debug_bounds("markdown-preview-slot").is_some());
        workbench.read_with(cx, |w, _| {
            assert_eq!(w.tab_titles().into_iter().map(|tab| tab.name).collect::<Vec<_>>(), ["README.md"]);
            assert!(w.editor().is_some());
        });
        let backend = cx.update(|_, cx| cx.global::<Backend>().host.clone());
        backend.close_project("w1");
    }

    #[gpui_kit::test]
    async fn reopening_a_project_restores_its_text_and_markdown_tabs_and_not_its_image_tab(cx: &mut TestAppContext) {
        use crate::features_b::viewers::image_view::ImageView;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("a.txt"), "a\n").expect("the file is written");
        std::fs::write(root.join("README.md"), "# Title\n").expect("the file is written");
        std::fs::write(root.join("logo.png"), PNG_2X2).expect("the image is written");
        let scope = root.to_string_lossy().into_owned();
        let key = format!("{}{scope}", session::STORAGE_PREFIX);
        let (first, cx1) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        first.update_in(cx1, |w, window, cx| w.open_project(root.clone(), window, cx));
        settle(cx1, |cx| first.read_with(cx, |w, _| w.project_tree().is_some()));
        for name in ["a.txt", "README.md"] {
            let path = root.join(name);
            first.update_in(cx1, |w, window, cx| w.open_project_file(path.clone(), window, cx));
            settle(cx1, |cx| first.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == path)));
        }
        first.update_in(cx1, |w, window, cx| w.open_project_file(root.join("logo.png"), window, cx));
        settle(cx1, |cx| {
            first.read_with(cx, |w, _| w.active_tab_view().and_then(|view| view.downcast::<ImageView>()).is_some())
        });
        first.update(cx1, |w, cx| w.save_session(cx));
        let stored: serde_json::Value = cx1
            .update(|_, cx| serde_json::from_str(cx.global::<UiStorage>().get(&key).expect("stored")))
            .expect("JSON");
        let tabs = stored["layout"]["tabs"].as_array().expect("one pane of tabs");
        let saved: Vec<String> = tabs.iter().filter_map(|tab| tab["path"].as_str()).map(str::to_owned).collect();
        let expected: Vec<String> =
            ["a.txt", "README.md"].iter().map(|name| root.join(name).to_string_lossy().into_owned()).collect();
        assert_eq!(saved, expected, "the image tab is not restorable");
        assert!(tabs.iter().all(|tab| tab.get("input").is_none()));
        cx1.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));

        let (second, cx2) = cx.add_window_view(|window, cx| Workbench::new("w2", window, cx));
        second.update_in(cx2, |w, window, cx| w.open_project(root.clone(), window, cx));
        settle(cx2, |cx| second.read_with(cx, |w, _| w.editor().is_some()));
        let names: Vec<String> = second.read_with(cx2, |w, _| w.tab_titles().into_iter().map(|t| t.name).collect());
        assert_eq!(names, ["a.txt", "README.md"]);
        // The Markdown tab gets its layouts back once it shows.
        second.update_in(cx2, |w, window, cx| w.with_tabs(window, cx, |tabs, pane| tabs.cycle_tab(1, Some(pane))));
        settle(cx2, |cx| second.read_with(cx, |w, _| w.active_markdown_host().is_some()));
        assert!(second.read_with(cx2, |w, _| w.active_tab_view().is_none() && w.editor().is_some()));
        cx2.update(|_, cx| cx.global::<Backend>().host.close_project("w2"));
    }

    /// A view tab and a loose file in a viewer have no text editor: the status bar leaves out the
    /// editor's items, as `StatusBar.tsx`'s items render nothing without one, and the title names
    /// the file the viewer shows.
    #[gpui_kit::test]
    async fn the_status_and_title_bars_name_the_viewers_file_and_leave_out_the_editors_items(cx: &mut TestAppContext) {
        use super::title_bar::TitleBarKind;
        use crate::features_b::viewers::image_view::ImageView;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("logo.png"), PNG_2X2).expect("the image is written");
        std::fs::write(root.join("notes.txt"), "a\n").expect("the file is written");
        let editor_items = [
            status_items::CARET,
            status_items::LINE_ENDING,
            status_items::ENCODING,
            status_items::INDENT,
            status_items::READ_ONLY,
        ];

        // A loose image: the single-file window's viewer.
        let (loose, cx1) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx1.update(|window, _| window.activate_window());
        loose.update_in(cx1, |w, window, cx| w.open_file(root.join("logo.png"), window, cx));
        settle(cx1, |cx| loose.read_with(cx, |w, _| w.active_tab_view().is_some() || w.failure().is_some()));
        assert!(loose.read_with(cx1, |w, _| matches!(w.content, Content::Viewer(_)) && w.editor().is_none()));
        assert!(loose.read_with(cx1, |w, _| w.active_tab_view().and_then(|v| v.downcast::<ImageView>()).is_some()));
        cx1.run_until_parked();
        assert!(context_is_on(cx1, "lightEdit") && context_is_on(cx1, "imageViewerActive"));
        assert!(context_is_on(cx1, "imageViewerFocus"), "the viewer has the keyboard");
        let title_bar = loose.read_with(cx1, |w, _| w.title_bar().clone());
        assert_eq!(title_bar.read_with(cx1, |bar, _| bar.kind()), TitleBarKind::LightEdit);
        assert!(loose.read_with(cx1, |w, cx| w.title(cx)).ends_with("logo.png"));
        let bar = loose.read_with(cx1, |w, _| w.status_bar().clone());
        let (left, right) = cx1.update(|window, cx| bar.read(cx).shown(window, cx));
        assert!(left.iter().chain(&right).all(|id| !editor_items.contains(&id.as_str())), "{left:?} {right:?}");
        // Open as Text from the viewer turns the window into a text editor of the file.
        loose.update_in(cx1, |w, window, cx| w.open_as_text(root.join("logo.png"), window, cx));
        settle(cx1, |cx| loose.read_with(cx, |w, _| w.editor().is_some() || w.failure().is_some()));
        assert!(loose.read_with(cx1, |w, _| matches!(w.content, Content::Editor(_))));
        cx1.run_until_parked();
        assert!(!context_is_on(cx1, "imageViewerActive"));
        cx1.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));

        // An image tab of a project, then a text tab: the editor's items come back with it.
        let (workbench, cx2) = cx.add_window_view(|window, cx| Workbench::new("w2", window, cx));
        workbench.update_in(cx2, |w, window, cx| w.open_project(root.clone(), window, cx));
        settle(cx2, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        workbench.update_in(cx2, |w, window, cx| w.open_project_file(root.join("logo.png"), window, cx));
        settle(cx2, |cx| workbench.read_with(cx, |w, _| w.active_tab_view().is_some()));
        cx2.run_until_parked();
        let title = workbench.read_with(cx2, |w, cx| w.title(cx));
        assert!(title.ends_with("logo.png"), "{title}");
        let bar = workbench.read_with(cx2, |w, _| w.status_bar().clone());
        let shown = |cx: &mut VisualTestContext| -> Vec<String> {
            let (left, right) = cx.update(|window, cx| bar.read(cx).shown(window, cx));
            left.into_iter().chain(right).filter(|id| editor_items.contains(&id.as_str())).collect()
        };
        assert_eq!(shown(cx2), Vec::<String>::new());
        workbench.update_in(cx2, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        settle(cx2, |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx2.run_until_parked();
        assert!(shown(cx2).contains(&status_items::CARET.to_owned()));
        assert!(workbench.read_with(cx2, |w, cx| w.title(cx)).ends_with("notes.txt"));
        cx2.update(|_, cx| cx.global::<Backend>().host.close_project("w2"));
    }

    /// The rest of the view tabs' wiring in one window: every image action, closing and
    /// reopening a view tab, splitting it, the hex view's Open as Text, a Markdown preview's local
    /// image and links, and closing a Markdown tab.
    #[gpui_kit::test]
    async fn view_tabs_run_their_actions_split_reopen_and_route_their_views_requests(cx: &mut TestAppContext) {
        use crate::features_b::viewers::file_kinds::IMAGE_ACTIONS;
        use crate::features_b::viewers::hex_view::{HexView, HexViewEvent};
        use crate::features_b::viewers::image_view::{ImageView, chessboard};
        use crate::features_b::viewers::markdown_view::MarkdownViewEvent;
        use crate::features_b::viewers::preview::ImageView as PreviewImage;
        use crate::platform::actions::execute_action_or_throw;
        let _home = start(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        std::fs::write(root.join("logo.png"), PNG_2X2).expect("the image is written");
        std::fs::write(root.join("data.bin"), b"\0\0binary payload").expect("the file is written");
        std::fs::write(root.join("other.txt"), "one\ntwo\nthree\n").expect("the file is written");
        std::fs::write(root.join("README.md"), "# Title\n\n![logo](logo.png)\n").expect("the file is written");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |w, window, cx| w.open_project(root.clone(), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        let run = |cx: &mut VisualTestContext, id: &str| {
            cx.update(|window, cx| execute_action_or_throw(id, None, window, cx)).expect("it runs");
            cx.run_until_parked();
        };
        let image = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.active_tab_view().and_then(|view| view.downcast::<ImageView>()))
        };

        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("logo.png"), window, cx));
        settle(cx, |cx| image(cx).is_some_and(|view| view.read_with(cx, |view, _| view.view().is_some())));
        let label = |cx: &mut VisualTestContext| image(cx).expect("the image tab").read_with(cx, |view, _| view.info());
        let board = cx.update(|_, cx| chessboard(cx));
        for id in IMAGE_ACTIONS {
            let before = label(cx);
            run(cx, id);
            if id == "image.zoomIn" {
                assert_ne!(label(cx), before, "{id}");
            }
        }
        assert_eq!(cx.update(|_, cx| chessboard(cx)), !board, "the chessboard toggled");
        run(cx, "image.toggleChessboard");
        assert_eq!(cx.update(|_, cx| chessboard(cx)), board);
        // The file size reaches the label once probed.
        settle(cx, |cx| label(cx).contains("74 B"));

        // Split: the copy in the new pane gets a view of its own.
        let first = image(cx).expect("the image tab");
        run(cx, "editor.splitRight");
        settle(cx, |cx| image(cx).is_some_and(|view| view != first));
        assert_eq!(workbench.read_with(cx, |w, _| w.tabs().map(|tabs| tabs.panes.len())), Some(2));
        run(cx, "editor.unsplitAll");
        assert_eq!(workbench.read_with(cx, |w, _| w.tabs().map(|tabs| tabs.panes.len())), Some(1));

        // Closing and reopening an image tab builds its view again.
        let names = |cx: &mut VisualTestContext| -> Vec<String> {
            workbench.read_with(cx, |w, _| w.tab_titles().into_iter().map(|tab| tab.name).collect())
        };
        run(cx, "editor.closeTab");
        assert!(cx.pending_prompt().is_none());
        workbench.update_in(cx, |w, window, cx| w.reopen_closed_tab(window, cx));
        settle(cx, |cx| image(cx).is_some());
        assert!(names(cx).contains(&"logo.png".to_owned()));

        // The hex view's Open as Text opens the file's text tab beside it.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("data.bin"), window, cx));
        let hex = |cx: &mut VisualTestContext| {
            workbench.read_with(cx, |w, _| w.active_tab_view().and_then(|view| view.downcast::<HexView>()))
        };
        settle(cx, |cx| hex(cx).is_some());
        let data = root.join("data.bin").to_string_lossy().into_owned();
        hex(cx).expect("the hex tab").update(cx, |_, cx| cx.emit(HexViewEvent::OpenAsText(data.clone())));
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == root.join("data.bin")))
        });
        assert_eq!(names(cx).iter().filter(|name| *name == "data.bin").count(), 2, "{:?}", names(cx));

        // A Markdown preview reads its local image, and its links open files.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("README.md"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.active_markdown_host().is_some()));
        let host = workbench.read_with(cx, |w, _| w.active_markdown_host()).expect("the Markdown layouts");
        run(cx, "markdown.editorAndPreview");
        let preview = host.read_with(cx, |host, _| host.preview().cloned()).expect("the preview shows");
        settle(cx, |cx| {
            preview.read_with(cx, |preview, cx| matches!(preview.image_view("logo.png", cx), PreviewImage::Shown(_)))
        });
        let other = root.join("other.txt");
        let target = other.to_string_lossy().into_owned();
        host.update(cx, |_, cx| cx.emit(MarkdownViewEvent::OpenFile { path: target.clone(), line: Some(2) }));
        settle(cx, |cx| workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).path() == other)));
        settle(cx, |cx| {
            workbench.read_with(cx, |w, cx| w.editor().is_some_and(|e| e.read(cx).caret_position().0 == 2))
        });

        // Closing the Markdown tab releases its layouts.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("README.md"), window, cx));
        settle(cx, |cx| workbench.read_with(cx, |w, _| w.active_markdown_host().is_some()));
        let weak = host.downgrade();
        drop((host, preview));
        run(cx, "editor.closeTab");
        assert!(workbench.read_with(cx, |w, _| w.active_markdown_host().is_none()));
        assert!(!names(cx).contains(&"README.md".to_owned()));
        assert!(weak.upgrade().is_none(), "the closed tab's layouts are released");
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// The project's SQLite data source `app` (id `s1`) on `app.db` in the project root, saved
    /// through the backend, with a `users` table of two rows (`1 ann`, `2 bob`).
    fn sqlite_source(root: &std::path::Path, cx: &mut VisualTestContext) {
        std::fs::File::create(root.join("app.db")).expect("the database file is made");
        let source = ide_db::DataSource {
            id: "s1".into(),
            name: "app".into(),
            driver: ide_db::DbDriver::Sqlite,
            file: Some(root.join("app.db").into()),
            ..Default::default()
        };
        cx.update(|_, cx| {
            let backend = cx.global::<Backend>();
            let host = backend.host.clone();
            backend.block_on(async move {
                ide_host::ops::db::save_sources(&host, "w1", vec![source]).await.expect("the source is saved");
            });
        });
        for statement in [
            "create table users (id integer primary key, name text not null)",
            "insert into users (name) values ('ann'), ('bob')",
        ] {
            db_query(cx, statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
    }

    /// Runs `text` on data source `s1` through the backend: the rows of its result, or its error.
    fn db_query(cx: &mut VisualTestContext, text: &str) -> Result<Vec<Vec<Option<String>>>, String> {
        let to = u32::try_from(text.encode_utf16().count()).expect("a short statement");
        let statement = ide_db::split::SqlStatement { from: 0, to, text: text.to_owned() };
        cx.update(|_, cx| {
            let backend = cx.global::<Backend>();
            let host = backend.host.clone();
            backend.block_on(async move {
                let request = Backend::next_request_id();
                let outcomes = ide_host::ops::db::execute(&host, "w1", request, "s1", vec![statement], 0, 1000, None)
                    .await
                    .map_err(|error| error.message)?;
                let outcome = outcomes.into_iter().next().ok_or_else(|| "no outcome".to_owned())?;
                match (outcome.result, outcome.error) {
                    (_, Some(error)) => Err(error),
                    (Some(result), None) => Ok(result.rows),
                    (None, None) => Ok(Vec::new()),
                }
            })
        })
    }

    fn db_tree(
        workbench: &Entity<Workbench>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::database::tree_view::DatabaseTree> {
        workbench.read_with(cx, |w, _| w.database_tree()).expect("the Database tools are built")
    }

    /// The key of the tree's first row labelled `label`, once it shows.
    fn db_row_key(
        tree: &Entity<crate::database::tree_view::DatabaseTree>,
        label: &str,
        cx: &mut VisualTestContext,
    ) -> String {
        let find = |cx: &mut VisualTestContext| {
            tree.read_with(cx, |tree, _| {
                tree.rows().iter().find(|row| row.label == label && !row.loading).map(|row| row.node.key.clone())
            })
        };
        wait_long(cx, label, |cx| find(cx).is_some());
        find(cx).unwrap_or_else(|| panic!("{label} shows"))
    }

    /// Expands the tree's row labelled `label`.
    fn db_expand(tree: &Entity<crate::database::tree_view::DatabaseTree>, label: &str, cx: &mut VisualTestContext) {
        let key = db_row_key(tree, label, cx);
        tree.update(cx, |tree, cx| tree.set_expanded(&key, true, cx));
        cx.run_until_parked();
    }

    /// The source list as the backend stores it: (name, driver).
    fn db_saved_sources(cx: &mut VisualTestContext) -> Vec<(String, ide_db::DbDriver)> {
        let sources = cx.update(|_, cx| ide_host::ops::db::sources(&cx.global::<Backend>().host, "w1"));
        sources.expect("the sources are read").into_iter().map(|source| (source.name, source.driver)).collect()
    }

    /// New Data Source opens the dialog. A SQLite file passes Test Connection, OK saves it as a
    /// data source of the project, and the tree shows it.
    #[gpui_kit::test]
    async fn new_data_source_tests_and_saves_a_sqlite_file_that_then_shows_in_the_tree(cx: &mut TestAppContext) {
        use crate::database::source_dialog::Field;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("README.md", "notes\n")]);
        std::fs::File::create(root.join("app.db")).expect("the database file is made");

        run_action(cx, "db.newDataSource", None);
        let dialog = workbench.read_with(cx, |w, _| w.database_source_dialog()).expect("the dialog is open");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.form().title()), "New Data Source");
        // The type list is a deferred popup and Browse… a native dialog: both are answered directly.
        let file = root.join("app.db").to_string_lossy().into_owned();
        dialog.update_in(cx, |dialog, window, cx| dialog.edit(|form| form.set_type("sqlite"), window, cx));
        dialog.update_in(cx, |dialog, window, cx| dialog.file_picked(Field::File, &file, window, cx));
        assert!(dialog.read_with(cx, |dialog, _| dialog.form().valid()));

        dialog.update(cx, |dialog, cx| dialog.press_test(cx));
        wait_long(cx, "Test Connection", |cx| dialog.read_with(cx, |dialog, _| dialog.form().test.is_some()));
        let test = dialog.read_with(cx, |dialog, _| dialog.form().test.clone()).expect("an outcome");
        assert!(test.ok && test.text.starts_with("Succeeded"), "{}", test.text);

        dialog.update(cx, |dialog, cx| dialog.press_ok(cx));
        wait_long(cx, "the save", |cx| workbench.read_with(cx, |w, _| w.database_source_dialog().is_none()));
        assert_eq!(db_saved_sources(cx), [("app".to_owned(), ide_db::DbDriver::Sqlite)]);
        let tree = db_tree(&workbench, cx);
        wait_long(cx, "the source in the tree", |cx| {
            tree.read_with(cx, |tree, _| tree.lines().iter().any(|line| line.starts_with("> app")))
        });
    }

    /// Database shows the tool window with the project's data sources. Expanding the source lists its
    /// schema's tables; expanding a table's columns lists them.
    #[gpui_kit::test]
    async fn expanding_a_data_source_lists_its_tables_and_a_tables_columns(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("README.md", "notes\n")]);
        sqlite_source(&root, cx);
        run_action(cx, "db.showToolWindow", None);
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(DATABASE_TOOL_WINDOW)));
        let tree = db_tree(&workbench, cx);
        for label in ["app", "main", "tables"] {
            db_expand(&tree, label, cx);
        }
        db_row_key(&tree, "users", cx);
        db_expand(&tree, "users", cx);
        db_expand(&tree, "columns", cx);
        // Source 0, schema 1, folder 2, table 3, group 4: a column is at depth 5.
        let column = |lines: &[String], name: &str| {
            let indented = format!("{}{name}", "  ".repeat(5));
            lines.iter().any(|line| line.starts_with(&indented))
        };
        wait_long(cx, "the columns", |cx| {
            let lines = tree.read_with(cx, |tree, _| tree.lines());
            column(&lines, "id") && column(&lines, "name")
        });
        let lines = tree.read_with(cx, |tree, _| tree.lines());
        assert!(lines.iter().any(|line| line.starts_with("      v users")), "{lines:?}");
        // The source is connected now.
        assert!(tree.read_with(cx, |tree, _| tree.data().connected.contains("s1")));
    }

    /// In Restricted Mode the Database tools reach no database: connecting, Test Connection and
    /// reading a source's schemas are refused with the backend's reason, and nothing connects. Once
    /// the project is trusted the same source connects and lists its schema.
    #[gpui_kit::test]
    async fn a_project_in_restricted_mode_connects_to_no_database_until_it_is_trusted(cx: &mut TestAppContext) {
        use crate::database::source_dialog::Field;
        const REFUSED: &str = "Project is in Restricted Mode. Trust the project to enable execution.";
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("README.md", "notes\n")]);
        // `app` (s1) gets a table; `other` (s2) is a second file, left unconnected.
        sqlite_source(&root, cx);
        std::fs::File::create(root.join("other.db")).expect("the database file is made");
        let sources: Vec<ide_db::DataSource> = ["s1", "s2"]
            .into_iter()
            .zip(["app", "other"])
            .map(|(id, name)| ide_db::DataSource {
                id: id.into(),
                name: name.into(),
                driver: ide_db::DbDriver::Sqlite,
                file: Some(root.join(format!("{name}.db")).into()),
                ..Default::default()
            })
            .collect();
        cx.update(|_, cx| {
            let backend = cx.global::<Backend>();
            let host = backend.host.clone();
            backend.block_on(async move {
                ide_host::ops::db::save_sources(&host, "w1", sources).await.expect("the sources are saved");
            });
        });
        run_action(cx, "db.showToolWindow", None);
        let tree = db_tree(&workbench, cx);
        // Trusted, `app` connects and lists its schema.
        db_expand(&tree, "app", cx);
        db_row_key(&tree, "main", cx);
        assert!(tree.read_with(cx, |tree, _| tree.data().connected.contains("s1")));

        run_action(cx, "project.revokeTrust", None);
        wait_long(cx, "Restricted Mode", |cx| workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        // The rows under the source `name`.
        let under = |name: &str, cx: &mut VisualTestContext| -> Vec<String> {
            let lines = tree.read_with(cx, |tree, _| tree.lines());
            let source = |line: &String| !line.starts_with(' ');
            let start = lines.iter().position(|line| source(line) && line[2..].starts_with(name));
            let Some(start) = start else { return Vec::new() };
            lines[start + 1..].iter().take_while(|line| !source(line)).map(|line| line.trim().to_owned()).collect()
        };
        // Connecting is refused.
        db_expand(&tree, "other", cx);
        wait_long(cx, "the refused connection", |cx| under("other", cx) == [REFUSED]);
        assert!(!tree.read_with(cx, |tree, _| tree.data().connected.contains("s2")));
        // Reading the schemas again is refused.
        let app = db_row_key(&tree, "app", cx);
        run_action(cx, "db.refresh", Some(serde_json::json!(app)));
        wait_long(cx, "the refused schema read", |cx| under("app", cx) == [REFUSED]);
        // Test Connection is refused.
        run_action(cx, "db.newDataSource", None);
        let dialog = workbench.read_with(cx, |w, _| w.database_source_dialog()).expect("the dialog is open");
        let file = root.join("other.db").to_string_lossy().into_owned();
        dialog.update_in(cx, |dialog, window, cx| dialog.edit(|form| form.set_type("sqlite"), window, cx));
        dialog.update_in(cx, |dialog, window, cx| dialog.file_picked(Field::File, &file, window, cx));
        dialog.update(cx, |dialog, cx| dialog.press_test(cx));
        wait_long(cx, "Test Connection", |cx| dialog.read_with(cx, |dialog, _| dialog.form().test.is_some()));
        let test = dialog.read_with(cx, |dialog, _| dialog.form().test.clone()).expect("an outcome");
        assert!(!test.ok && test.text.contains(REFUSED), "{}", test.text);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // Trusted again from the banner, `other` connects and lists its schema.
        click_on(cx, "restricted-banner-trust");
        wait_long(cx, "the trust", |cx| workbench.read_with(cx, |w, cx| !w.project_restricted(cx)));
        let other = db_row_key(&tree, "other", cx);
        run_action(cx, "db.refresh", Some(serde_json::json!(other)));
        wait_long(cx, "the schema of other", |cx| under("other", cx) == ["> main"]);
        assert!(tree.read_with(cx, |tree, _| tree.data().connected.contains("s2")));
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// Edit Data opens the table's rows in the table editor. Submit writes an edited cell to the
    /// table; Revert discards an edit.
    #[gpui_kit::test]
    async fn edit_data_shows_the_rows_submit_writes_an_edit_and_revert_discards_one(cx: &mut TestAppContext) {
        use crate::database::grid::CellEdit;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("README.md", "notes\n")]);
        // Submit and Revert follow the keyboard: focus is reported in an active window only.
        cx.update(|window, _| window.activate_window());
        sqlite_source(&root, cx);
        run_action(cx, "db.showToolWindow", None);
        let tree = db_tree(&workbench, cx);
        for label in ["app", "main", "tables"] {
            db_expand(&tree, label, cx);
        }
        let users = db_row_key(&tree, "users", cx);
        run_action(cx, "db.editData", Some(serde_json::Value::from(users)));
        let editor = workbench.read_with(cx, |w, cx| w.database_table_editor(cx)).expect("the table editor shows");
        let rows = |cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, _| editor.state().page.as_ref().map(|page| page.result.rows.clone()))
        };
        wait_long(cx, "the rows", |cx| rows(cx).is_some());
        let cell = |text: &str| Some(text.to_owned());
        assert_eq!(rows(cx), Some(vec![vec![cell("1"), cell("ann")], vec![cell("2"), cell("bob")]]));
        assert!(editor.read_with(cx, |editor, _| editor.is_editable()));
        let panel = workbench.read_with(cx, |w, _| w.database_panel()).expect("the panel");
        let id = editor.read_with(cx, |editor, _| editor.id());
        assert_eq!(panel.read_with(cx, |panel, _| panel.lower()), super::database::Lower::Table(id));
        // The table editor has the keyboard, with nothing changed yet.
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(context_is_on(cx, "dbTableEditorFocus") && !context_is_on(cx, "dbTableEditorChanges"));

        editor.update(cx, |editor, cx| editor.edit(&[CellEdit { row: 0, col: 1, value: cell("zed") }], cx));
        cx.run_until_parked();
        assert!(editor.read_with(cx, |editor, _| editor.is_dirty()));
        assert!(context_is_on(cx, "dbTableEditorChanges"), "an edit waits to be submitted");
        run_action(cx, "db.submitChanges", None);
        let written = |cx: &mut VisualTestContext| db_query(cx, "select name from users where id = 1");
        wait_long(cx, "the write", |cx| written(cx) == Ok(vec![vec![cell("zed")]]));
        wait_long(cx, "the page read again", |cx| {
            editor.read_with(cx, |editor, _| !editor.is_loading() && !editor.is_dirty())
        });
        assert_eq!(rows(cx).and_then(|rows| rows.first().cloned()), Some(vec![cell("1"), cell("zed")]));
        assert!(!context_is_on(cx, "dbTableEditorChanges"), "nothing waits once submitted");

        editor.update(cx, |editor, cx| editor.edit(&[CellEdit { row: 1, col: 1, value: cell("yan") }], cx));
        cx.run_until_parked();
        assert_eq!(editor.read_with(cx, |editor, _| editor.state().value(1, 1)), cell("yan"));
        run_action(cx, "db.revertChanges", None);
        assert!(!editor.read_with(cx, |editor, _| editor.is_dirty()));
        assert_eq!(editor.read_with(cx, |editor, _| editor.state().value(1, 1)), cell("bob"));
        assert_eq!(db_query(cx, "select name from users where id = 2"), Ok(vec![vec![cell("bob")]]));
        // The Database tree takes the keyboard: the table editor's keys go off.
        cx.update(|window, cx| window.focus(&tree.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(!context_is_on(cx, "dbTableEditorFocus") && context_is_on(cx, "databaseViewFocus"));
    }

    /// Execute SQL Statement in a `.sql` editor runs the statement at the caret on the project's
    /// only data source. Its rows show in the result grid, and the Output tab logs the statement and
    /// its summary. A failing statement logs its error.
    #[gpui_kit::test]
    async fn execute_shows_a_querys_rows_and_its_output_lines_and_a_failing_statement_its_error(
        cx: &mut TestAppContext,
    ) {
        use crate::database::result_session::OutputLine;
        let _home = start(cx);
        let query = "select name from users order by id";
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("query.sql", query)]);
        sqlite_source(&root, cx);
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("query.sql"), window, cx));
        wait_long(cx, "the editor", |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        let end = u32::try_from(query.encode_utf16().count()).expect("a short query");
        editor.update(cx, |editor, cx| editor.select(end, end, cx));

        run_action(cx, "db.execute", None);
        let results = workbench.read_with(cx, |w, _| w.database_results()).expect("the result pane");
        wait_long(cx, "the result", |cx| results.read_with(cx, |results, _| results.tabs().len() == 1));
        let rows = results.read_with(cx, |results, _| results.tabs()[0].origin.result.rows.clone());
        assert_eq!(rows, [[Some("ann".to_owned())], [Some("bob".to_owned())]]);
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(DATABASE_TOOL_WINDOW)));
        let panel = workbench.read_with(cx, |w, _| w.database_panel()).expect("the panel");
        assert_eq!(panel.read_with(cx, |panel, _| panel.lower()), super::database::Lower::Results);
        let output = results.read_with(cx, |results, _| results.output().to_vec());
        let statement =
            |line: &OutputLine| matches!(line, OutputLine::Statement(text) if text.starts_with("main> select"));
        assert!(output.iter().any(statement), "{output:?}");
        assert!(
            output.iter().any(|line| matches!(line, OutputLine::Result(text) if text.starts_with("2 rows retrieved"))),
            "{output:?}"
        );

        let failing = "select * from missing";
        editor.update(cx, |editor, cx| {
            editor.insert(Some(0..end), failing, cx);
        });
        run_action(cx, "db.execute", None);
        wait_long(cx, "the error", |cx| {
            results.read_with(cx, |results, _| {
                results.output().iter().any(|line| matches!(line, OutputLine::Error(text) if text.contains("missing")))
            })
        });
        assert_eq!(results.read_with(cx, |results, _| results.tabs().len()), 1, "an error opens no result tab");
    }

    /// Drop on a data source asks first. Cancel keeps it; Remove deletes it from the project and
    /// the tree.
    #[gpui_kit::test]
    async fn dropping_a_data_source_asks_and_removes_it_from_the_project_and_the_tree(cx: &mut TestAppContext) {
        use crate::vcs::prompt_dialogs::PromptDialog;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("README.md", "notes\n")]);
        sqlite_source(&root, cx);
        run_action(cx, "db.showToolWindow", None);
        let tree = db_tree(&workbench, cx);
        let source = db_row_key(&tree, "app", cx);
        let prompt = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.database_prompt::<PromptDialog>());

        run_action(cx, "db.drop", Some(serde_json::Value::from(source.clone())));
        let dialog = prompt(cx).expect("the question is open");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.title().to_owned()), "Remove Data Source");
        click_on(cx, "prompt-button-cancel");
        assert!(prompt(cx).is_none());
        assert_eq!(db_saved_sources(cx).len(), 1);

        run_action(cx, "db.drop", Some(serde_json::Value::from(source)));
        assert!(prompt(cx).is_some());
        click_on(cx, "prompt-button-ok");
        wait_long(cx, "the removal", |cx| db_saved_sources(cx).is_empty());
        wait_long(cx, "the tree without it", |cx| tree.read_with(cx, |tree, _| tree.rows().is_empty()));
        assert_eq!(tree.update(cx, |tree, cx| tree.empty_text(cx)), Some("No data sources".to_owned()));
    }

    /// The rest of the Database wiring in one window: the tree's shortcuts work only while it has
    /// the keyboard (F4 opens the table there, and not in an editor), Copy Reference, Rename,
    /// Duplicate, Refresh, Disconnect, Collapse All, Properties, Go to DDL, the console and
    /// Explain Plan.
    #[gpui_kit::test]
    async fn the_database_trees_actions_and_shortcuts_reach_the_store_and_the_backend(cx: &mut TestAppContext) {
        use crate::vcs::prompt_dialogs::PromptDialog;
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("notes.txt", "notes\n")]);
        cx.update(|window, _| window.activate_window());
        sqlite_source(&root, cx);
        run_action(cx, "db.showToolWindow", None);
        let tree = db_tree(&workbench, cx);
        for label in ["app", "main", "tables"] {
            db_expand(&tree, label, cx);
        }
        let users = db_row_key(&tree, "users", cx);
        let source = db_row_key(&tree, "app", cx);

        // F4 in a text editor is not Edit Data.
        workbench.update_in(cx, |w, window, cx| w.open_project_file(root.join("notes.txt"), window, cx));
        wait_long(cx, "the editor", |cx| workbench.read_with(cx, |w, _| w.editor().is_some()));
        cx.run_until_parked();
        assert!(!context_is_on(cx, "databaseViewFocus"));
        cx.simulate_keystrokes("f4");
        assert!(workbench.read_with(cx, |w, cx| w.database_table_editor(cx)).is_none());
        // In the tree, F4 opens the selected table.
        tree.update_in(cx, |tree, window, cx| {
            tree.select(&users, cx);
            window.focus(&tree.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        assert!(context_is_on(cx, "databaseViewFocus"));
        cx.simulate_keystrokes("f4");
        let editor = workbench.read_with(cx, |w, cx| w.database_table_editor(cx)).expect("F4 opens the table");
        wait_long(cx, "the rows", |cx| editor.read_with(cx, |editor, _| editor.state().page.is_some()));

        // Copy Reference of the table and of the source.
        run_action(cx, "db.copyReference", Some(serde_json::Value::from(users.clone())));
        let copied = cx.read_from_clipboard().and_then(|item| item.text()).expect("a reference is copied");
        assert!(copied.contains("users"), "{copied}");
        run_action(cx, "db.copyReference", Some(serde_json::Value::from(source.clone())));
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("app"));

        // Rename asks for the new name and saves it.
        run_action(cx, "db.rename", Some(serde_json::Value::from(source.clone())));
        let prompt = workbench.read_with(cx, |w, _| w.database_prompt::<PromptDialog>()).expect("the question");
        assert_eq!(prompt.read_with(cx, |dialog, _| dialog.title().to_owned()), "Rename Data Source");
        let field = prompt.read_with(cx, |dialog, _| dialog.field().cloned()).expect("a name field");
        cx.update(|window, cx| window.focus(&field.focus_handle(cx), cx));
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("main db");
        click_on(cx, "prompt-button-ok");
        wait_long(cx, "the rename", |cx| db_saved_sources(cx) == [("main db".to_owned(), ide_db::DbDriver::Sqlite)]);
        // The store has the new name once it has read the sources again: the tree shows it.
        db_row_key(&tree, "main db", cx);

        // Duplicate adds a second source; Refresh and Disconnect go through.
        run_action(cx, "db.duplicateSource", Some(serde_json::Value::from(source.clone())));
        wait_long(cx, "the copy", |cx| db_saved_sources(cx).len() == 2);
        wait_long(cx, "the copy in the tree", |cx| {
            tree.read_with(cx, |tree, _| tree.rows().iter().filter(|row| row.depth == 0).count() == 2)
        });
        run_action(cx, "db.refresh", Some(serde_json::Value::from(source.clone())));
        db_row_key(&tree, "users", cx);
        run_action(cx, "db.disconnect", Some(serde_json::Value::from(source.clone())));
        wait_long(cx, "the disconnect", |cx| tree.read_with(cx, |tree, _| !tree.data().connected.contains("s1")));
        run_action(cx, "db.collapseAll", None);
        assert!(!tree.read_with(cx, |tree, _| tree.is_expanded(&source)));

        // Properties opens the dialog over the source; Cancel closes it.
        run_action(cx, "db.properties", Some(serde_json::Value::from(source.clone())));
        let dialog = workbench.read_with(cx, |w, _| w.database_source_dialog()).expect("the dialog is open");
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.form().source.name.clone()), "main db");
        click_on(cx, "source-cancel");
        assert!(workbench.read_with(cx, |w, _| w.database_source_dialog()).is_none());

        // Go to DDL opens the table's SQL in an editor.
        for label in ["main db", "main", "tables"] {
            db_expand(&tree, label, cx);
        }
        let users = db_row_key(&tree, "users", cx);
        run_action(cx, "db.goToDdl", Some(serde_json::Value::from(users)));
        wait_long(cx, "the DDL", |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| e.read(cx).text().to_lowercase().contains("create table"))
            })
        });

        // The console opens; Explain Plan runs its statement.
        run_action(cx, "db.openConsole", Some(serde_json::Value::from(source)));
        wait_long(cx, "the console", |cx| {
            workbench.read_with(cx, |w, cx| {
                w.editor().is_some_and(|e| e.read(cx).path().extension().is_some_and(|x| x == "sql"))
            })
        });
        let console = workbench.read_with(cx, |w, _| w.editor().cloned()).expect("the console");
        let query = "select name from users";
        console.update(cx, |editor, cx| {
            editor.insert(None, query, cx);
        });
        cx.run_until_parked();
        run_action(cx, "db.explainPlan", None);
        run_action(cx, "db.execute", None);
        let results = workbench.read_with(cx, |w, _| w.database_results()).expect("the result pane");
        wait_long(cx, "the console's result", |cx| results.read_with(cx, |results, _| !results.tabs().is_empty()));
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// The password a connection asks for is typed in a masked field: it paints bullets, Copy and
    /// Cut copy nothing, and Enter hands it to the store as a secret whose `Debug` output hides it.
    #[gpui_kit::test]
    async fn the_password_a_connection_asks_for_is_masked_and_answers_the_store(cx: &mut TestAppContext) {
        use super::database::{FieldsPrompt, FieldsPromptEvent, secret_answer};
        use crate::database::store::{Prompt, PromptAnswer};
        use std::cell::RefCell;
        use std::rc::Rc;
        const SECRET: &str = "Zq7#hunter";
        let _home = start(cx);
        let (_folder, root, workbench, cx) = trusted_project(cx, &[("README.md", "notes\n")]);
        cx.update(|window, _| window.activate_window());
        sqlite_source(&root, cx);
        run_action(cx, "db.showToolWindow", None);
        let source = ide_db::DataSource { id: "pg".into(), name: "warehouse".into(), ..Default::default() };
        workbench.update_in(cx, |w, window, cx| {
            w.ask_database(7, Prompt::Password { source: Box::new(source) }, window, cx);
        });
        cx.run_until_parked();
        let prompt = workbench.read_with(cx, |w, _| w.database_prompt::<FieldsPrompt>()).expect("the question");
        let answers: Rc<RefCell<Vec<FieldsPromptEvent>>> = Rc::default();
        let seen = Rc::clone(&answers);
        cx.update(|_, cx| {
            cx.subscribe(&prompt, move |_, event: &FieldsPromptEvent, _| seen.borrow_mut().push(event.clone()))
                .detach();
        });
        let field = prompt.read_with(cx, |prompt, _| prompt.secret_input()).expect("a masked field");
        assert!(cx.update(|window, cx| field.focus_handle(cx).is_focused(window)), "it has the keyboard");
        cx.simulate_input(SECRET);
        assert_eq!(field.read_with(cx, |field, cx| field.text(cx).to_owned()), SECRET);
        let painted = field.read_with(cx, |field, cx| field.painted(cx));
        assert!(painted.iter().all(|part| part.chars().all(|c| c == '\u{2022}')), "{painted:?}");
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("before".to_owned()));
        cx.simulate_keystrokes("cmd-a cmd-c cmd-x");
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("before"));

        cx.simulate_keystrokes("enter");
        assert!(workbench.read_with(cx, |w, _| w.database_prompt::<FieldsPrompt>()).is_none(), "the question closed");
        let Some(FieldsPromptEvent::Done { values, checked }) = answers.borrow().last().cloned() else {
            panic!("the question was answered")
        };
        assert_eq!(values, [SECRET]);
        let answer = secret_answer(values, checked);
        assert!(matches!(&answer, PromptAnswer::Secret { value, .. } if value.expose() == SECRET));
        let printed = format!("{answer:?}");
        assert!(!printed.contains(SECRET), "{printed}");
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }
}
