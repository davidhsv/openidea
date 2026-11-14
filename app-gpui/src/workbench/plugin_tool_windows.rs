//! The views plugins declare (`[[contributes.views]]`) as tool windows of the window's project.
//!
//! - **Windows.** A view is a tool window `plugin.<plugin>.<view>`; the views a plugin puts in one
//!   group share `plugin.<plugin>.group:<group>`, a tab each in declaration order. A view shows while
//!   its `when` holds in the window; a window shows while one of its views does, titled, placed and
//!   drawn with the icon of its first declared view (shown or not), after the IDE's own windows.
//!   Each window has its `toolwindow.<id>` action. A view declared for the Services tool window is
//!   not one of these.
//! - **Content.** A tab gets its panel the first time it shows, and only then is the plugin asked to
//!   render it (which loads its code): registering a window runs nothing of the plugin. A render
//!   runs off the main thread under the window's project and focused editor, as
//!   `plugin_view_render` does; what the user does in it reaches the plugin's `handle` in order,
//!   one event after the other, as `plugin_view_event` does, and a failure shows above the view.
//! - **Plugin code.** `refresh` renders the window's panels of the view again, `reveal` shows (and
//!   focuses) the view's window with its tab chosen when the view is shown, and `post-message` is
//!   for a web page, which this window does not draw.
//! - **Lifecycle.** A view that is no longer declared (its plugin disabled, uninstalled, or
//!   reloaded without it) loses its window and its panel; a declaration that changed is shown
//!   anew. Closing the project drops them all; the next project registers them again.
//!
//! A view's SVG icon (`icon = "….svg"`) is read from the plugin once and drawn in place of the
//! generic plugin icon from then on; a plugin that went meanwhile is not brought back by it.

use super::plugin_runtime::plugin_host;
use super::services_window::on_workbench;
use super::tool_windows::{Anchor, Effects, Registration, ToolWindow};
use super::{Content, Panel, Workbench};
use crate::backend::Backend;
use crate::platform::actions::{Action, ActionMeta, ActionRegistry};
use crate::platform::context::{
    ContextKeys, ContextStore, clear_context, context_changes, evaluate_when, window_context,
};
use crate::plugin_models::contribution::Contribution;
use crate::plugin_models::plugin_icon::{GENERIC_ICON, standalone};
use crate::plugin_views::view_node::{TableRowCommand, table_row_menu};
use crate::plugin_views::view_panel::{
    PluginToolWindowView, PluginViewEvent, PluginViewPanel, PluginWindowEvent, TableMenu,
};
use crate::plugins::{Plugins, plugin_action_id};
use crate::vcs_ops::menu::{MenuEntry, MenuItem};
use gpui_kit::{App, AppContext as _, Context, Entity, Global, Pixels, Point, Subscription, Window};
use ide_core::{IdeError, IdeResult};
use ide_host::plugins::PluginsSnapshot;
use ide_plugin::{View, ViewBody, ViewEvent};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// The extension point of declared views.
const VIEWS_POINT: &str = "views";
const MENUS_POINT: &str = "menus";
/// The menu of a view's items.
const VIEW_ITEM_MENU: &str = "viewItem.context";
/// A view shown in the Services tool window instead of a window of its own.
const SERVICES_LOCATION: &str = "services";
/// Plugin windows come after the IDE's own on their stripe.
const PLUGIN_WINDOW_ORDER: f64 = 900.0;
const TOOL_WINDOW_ACTION_PREFIX: &str = "toolwindow.";

/// A plugin's view and the view of a window's tab, by (plugin, view).
pub(super) type ViewKey = (String, String);

/// A view as its plugin declares it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DeclaredView {
    pub plugin_id: String,
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    pub anchor: Anchor,
    pub secondary: bool,
    pub group: Option<String>,
    pub when: Option<String>,
    /// A web page of the package, which this window does not draw.
    pub web: bool,
}

/// A plugin tool window: its id and the views it holds, in declaration order.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct WindowSpec {
    pub id: String,
    pub plugin_id: String,
    pub grouped: bool,
    pub members: Vec<DeclaredView>,
}

impl WindowSpec {
    /// The view the window is titled, placed and drawn after.
    fn first(&self) -> &DeclaredView {
        &self.members[0]
    }

    /// The views whose `when` holds in `ctx`; a clause that does not parse does not.
    fn shown(&self, ctx: &ContextKeys) -> Vec<&DeclaredView> {
        self.members.iter().filter(|view| evaluate_when(view.when.as_deref(), ctx).unwrap_or(false)).collect()
    }
}

/// The tool window of plugin `plugin`'s ungrouped view `view`.
pub fn plugin_tool_window_id(plugin: &str, view: &str) -> String {
    format!("plugin.{plugin}.{view}")
}

/// The tool window of plugin `plugin`'s view group `group`; group names cannot hold `:`, so it
/// cannot be an ungrouped view's.
pub fn plugin_group_window_id(plugin: &str, group: &str) -> String {
    format!("plugin.{plugin}.group:{group}")
}

/// The declared views of a listing that get a tool window, in their order.
pub(super) fn declared_views(contributions: &[Contribution]) -> Vec<DeclaredView> {
    contributions
        .iter()
        .filter(|c| c.point == VIEWS_POINT)
        .filter_map(|c| {
            let anchor = match c.text("location").unwrap_or("right") {
                "left" => Anchor::Left,
                "right" => Anchor::Right,
                "bottom" => Anchor::Bottom,
                // A view of the Services tool window has no window of its own.
                SERVICES_LOCATION => return None,
                // A place this build does not know.
                _ => return None,
            };
            Some(DeclaredView {
                plugin_id: c.plugin_id.clone(),
                id: c.text("id")?.to_owned(),
                title: c.text("title").unwrap_or_default().to_owned(),
                icon: c.text("icon").map(str::to_owned),
                anchor,
                secondary: c.decl.get("secondary").and_then(Value::as_bool) == Some(true),
                group: c.text("group").filter(|group| !group.is_empty()).map(str::to_owned),
                when: c.when().map(str::to_owned),
                web: c.decl.get("web").is_some_and(|web| !web.is_null()),
            })
        })
        .collect()
}

/// The plugin tool windows of a listing, a window per ungrouped view and per (plugin, group), in
/// the order their first view is declared.
pub(super) fn window_specs(contributions: &[Contribution]) -> Vec<WindowSpec> {
    let mut specs: Vec<WindowSpec> = Vec::new();
    for view in declared_views(contributions) {
        let (id, grouped) = match &view.group {
            Some(group) => (plugin_group_window_id(&view.plugin_id, group), true),
            None => (plugin_tool_window_id(&view.plugin_id, &view.id), false),
        };
        match specs.iter_mut().find(|spec| spec.id == id) {
            Some(spec) => spec.members.push(view),
            None => specs.push(WindowSpec { id, plugin_id: view.plugin_id.clone(), grouped, members: vec![view] }),
        }
    }
    specs
}

/// What a window is registered as: its title, side, split area and icon. A change registers it
/// again.
type Signature = (String, Anchor, bool, String);

/// A window to register: its declaration, how it is registered, and its shown views (view, title).
type Wanted = (WindowSpec, Signature, Vec<(String, String)>);

/// The contributions of the live plugins in `listed`.
fn listed_contributions(listed: Option<&PluginsSnapshot>) -> Vec<Contribution> {
    listed.map(Contribution::from_snapshot).unwrap_or_default()
}

fn merge(effects: &mut Effects, more: Effects) {
    effects.persist |= more.persist;
    effects.focus_editor |= more.focus_editor;
}

/// The `toolwindow.<id>` actions registered for plugin windows, with their (title, icon). Plugin
/// windows are the same in every window; the action acts in the window it runs in.
#[derive(Default)]
struct PluginWindowActions(BTreeMap<String, (String, String)>);

impl Global for PluginWindowActions {}

/// A registered plugin window.
struct WindowState {
    spec: WindowSpec,
    signature: Signature,
    /// The views shown, (view, title).
    members: Vec<(String, String)>,
    /// `None` while the registration is under way.
    registration: Option<Registration>,
}

/// A view's panel and what it waits on.
struct ViewSlot {
    window: String,
    panel: Entity<PluginViewPanel>,
    /// Events for the plugin's `handle`, the next first; one is delivered at a time.
    queue: VecDeque<ViewEvent>,
    sending: bool,
    _events: Subscription,
}

/// The plugin tool windows of the window's project.
#[derive(Default)]
pub(super) struct PluginToolWindows {
    windows: BTreeMap<String, WindowState>,
    /// The tab chosen in each window, kept while its view is not mounted.
    selected: HashMap<String, String>,
    containers: HashMap<String, Entity<PluginToolWindowView>>,
    views: HashMap<ViewKey, ViewSlot>,
    /// The registered SVG icon of each view that has one, and the views whose icon was asked for.
    icons: HashMap<ViewKey, String>,
    icons_asked: HashSet<ViewKey>,
    /// The context changes last judged.
    context_seen: Option<u64>,
    observers: Vec<Subscription>,
}

impl Workbench {
    // ---------------------------------------------------------------------------------------------
    // Registration

    /// A project opened: its plugin windows are registered, and follow the listing and the
    /// window's context keys from now on.
    pub(super) fn mount_plugin_tool_windows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugin_tool_windows.observers.is_empty() {
            let listed = cx.observe_global_in::<Plugins>(window, |workbench, window, cx| {
                workbench.sync_plugin_tool_windows(window, cx)
            });
            let context = cx.observe_global_in::<ContextStore>(window, |workbench, window, cx| {
                if workbench.plugin_tool_windows.context_seen != Some(context_changes(cx)) {
                    workbench.sync_plugin_tool_windows(window, cx);
                }
            });
            self.plugin_tool_windows.observers = vec![listed, context];
        }
        self.sync_plugin_tool_windows(window, cx);
    }

    /// The project goes: its plugin windows, their panels and what they wait on go with it.
    pub(super) fn drop_plugin_tool_windows(&mut self) {
        let slot = &mut self.plugin_tool_windows;
        slot.windows.clear();
        slot.selected.clear();
        slot.containers.clear();
        slot.views.clear();
        slot.context_seen = None;
    }

    /// The icon a view's window is drawn with: its registered SVG, its icon name, or the plugin icon.
    fn plugin_view_icon(&self, view: &DeclaredView) -> String {
        match view.icon.as_deref() {
            Some(icon) if icon.ends_with(".svg") => {
                let key = (view.plugin_id.clone(), view.id.clone());
                self.plugin_tool_windows.icons.get(&key).cloned().unwrap_or_else(|| GENERIC_ICON.to_owned())
            }
            Some(icon) => icon.to_owned(),
            None => GENERIC_ICON.to_owned(),
        }
    }

    /// Brings the registered plugin windows in line with the listing and the window's context keys.
    pub(super) fn sync_plugin_tool_windows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let specs = window_specs(&listed_contributions(self.plugin_listing(cx)));
        let titled: Vec<(String, String, String)> = specs
            .iter()
            .map(|spec| (spec.id.clone(), spec.first().title.clone(), self.plugin_view_icon(spec.first())))
            .collect();
        sync_window_actions(&titled, cx);
        if !matches!(self.content, Content::Project { .. }) {
            return;
        }
        self.plugin_tool_windows.context_seen = Some(context_changes(cx));
        let ctx = window_context(window, cx);
        for spec in &specs {
            self.ask_plugin_view_icons(spec, window, cx);
        }
        let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
        for spec in specs {
            let members: Vec<(String, String)> =
                spec.shown(&ctx).into_iter().map(|view| (view.id.clone(), view.title.clone())).collect();
            if members.is_empty() {
                continue;
            }
            let first = spec.first();
            let signature = (first.title.clone(), first.anchor, first.secondary, self.plugin_view_icon(first));
            wanted.insert(spec.id.clone(), (spec, signature, members));
        }
        // Windows no longer wanted, or registered otherwise now.
        let stale: Vec<String> = self
            .plugin_tool_windows
            .windows
            .iter()
            .filter(|(id, state)| wanted.get(*id).is_none_or(|(_, signature, _)| *signature != state.signature))
            .map(|(id, _)| id.clone())
            .collect();
        let mut unregister = Vec::new();
        for id in &stale {
            if let Some(registration) = self.plugin_tool_windows.windows.remove(id).and_then(|state| state.registration)
            {
                unregister.push(registration);
            }
            self.drop_plugin_window_panels(id);
            clear_context(window, cx, &format!("toolWindow.{id}.visible"));
        }
        let mut added: Vec<(String, ToolWindow)> = Vec::new();
        for (id, (spec, signature, members)) in wanted {
            if let Some(state) = self.plugin_tool_windows.windows.get_mut(&id) {
                if state.members != members || state.spec != spec {
                    // A view withdrawn or declared anew loses its panel.
                    let kept: Vec<String> = members
                        .iter()
                        .filter(|(view, _)| {
                            let old = state.spec.members.iter().find(|m| m.id == *view);
                            old.is_some() && old == spec.members.iter().find(|m| m.id == *view)
                        })
                        .map(|(view, _)| view.clone())
                        .collect();
                    state.members = members.clone();
                    state.spec = spec;
                    let plugin = state.spec.plugin_id.clone();
                    self.plugin_tool_windows
                        .views
                        .retain(|(owner, view), slot| slot.window != id || (*owner == plugin && kept.contains(view)));
                    if let Some(container) = self.plugin_tool_windows.containers.get(&id).cloned() {
                        let kept_members: Vec<(String, String)> =
                            members.iter().filter(|(view, _)| kept.contains(view)).cloned().collect();
                        // Dropped tabs lose their panel first; the others keep theirs.
                        container.update(cx, |container, cx| {
                            container.set_members(kept_members, cx);
                            container.set_members(members, cx);
                        });
                        let active = container.read(cx).active().map(str::to_owned);
                        if let Some(active) = active {
                            self.ensure_plugin_view(&id, &active, window, cx);
                        }
                    }
                }
                continue;
            }
            let first = spec.first();
            let declared = ToolWindow {
                order: Some(PLUGIN_WINDOW_ORDER),
                secondary: first.secondary,
                verbatim: true,
                ..ToolWindow::new(&id, &first.title, &signature.3, first.anchor)
            };
            self.plugin_tool_windows
                .windows
                .insert(id.clone(), WindowState { spec, signature, members, registration: None });
            added.push((id, declared));
        }
        if unregister.is_empty() && added.is_empty() {
            return;
        }
        let mut registered: Vec<(String, Registration)> = Vec::new();
        self.with_layout(window, cx, |layout| {
            let mut effects = Effects::default();
            for registration in unregister {
                merge(&mut effects, layout.unregister(registration));
            }
            for (id, declared) in added {
                let (registration, more) = layout.register(declared);
                merge(&mut effects, more);
                registered.push((id, registration));
            }
            effects
        });
        for (id, registration) in registered {
            if let Some(state) = self.plugin_tool_windows.windows.get_mut(&id) {
                state.registration = Some(registration);
            }
        }
        cx.notify();
    }

    /// Forgets the panels of window `id`.
    fn drop_plugin_window_panels(&mut self, id: &str) {
        let slot = &mut self.plugin_tool_windows;
        slot.containers.remove(id);
        slot.views.retain(|_, view| view.window != id);
        if let Content::Project { panels, .. } = &mut self.content {
            panels.remove(id);
        }
    }

    /// Reads the SVG icons of `spec`'s views that have one, once each.
    fn ask_plugin_view_icons(&mut self, spec: &WindowSpec, window: &mut Window, cx: &mut Context<Self>) {
        for view in &spec.members {
            if !view.icon.as_deref().is_some_and(|icon| icon.ends_with(".svg")) {
                continue;
            }
            let key = (view.plugin_id.clone(), view.id.clone());
            if !self.plugin_tool_windows.icons_asked.insert(key.clone()) {
                continue;
            }
            let host = plugin_host(cx);
            let asset = ide_host::plugins::ui_points::view_icon_asset(&view.id);
            let (plugin, wanted) = (view.plugin_id.clone(), asset.clone());
            let read = cx.background_executor().spawn(async move {
                let assets = host?.assets(&plugin, &wanted).ok()?;
                assets.into_iter().find(|(id, _)| *id == wanted).map(|(_, svg)| svg)
            });
            cx.spawn_in(window, async move |workbench, cx| {
                let Some(svg) = read.await else { return };
                let _ = workbench.update_in(cx, |workbench, window, cx| {
                    // A plugin that went meanwhile is not brought back by its icon.
                    if !workbench.plugin_tool_windows.windows.values().any(|state| state.spec.plugin_id == key.0) {
                        return;
                    }
                    let name = format!("ext:{}/view-{}", key.0, key.1);
                    crate::theme::icons::register_external_icons([(name.clone(), standalone(&svg).into_bytes())]);
                    workbench.plugin_tool_windows.icons.insert(key, name);
                    workbench.sync_plugin_tool_windows(window, cx);
                });
            })
            .detach();
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Panels

    /// The view of plugin window `id`, made when it is first shown: its tabs, and the panel of the
    /// one chosen. `None` for an id that is not a plugin window.
    pub(super) fn mount_plugin_tool_window(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Panel> {
        let state = self.plugin_tool_windows.windows.get(id)?;
        let (grouped, members) = (state.spec.grouped, state.members.clone());
        let selected = self.plugin_tool_windows.selected.get(id).cloned();
        let container = cx.new(|cx| PluginToolWindowView::new(grouped, members, selected, cx));
        let window_id = id.to_owned();
        let events = cx.subscribe_in(&container, window, move |workbench, _, event: &PluginWindowEvent, window, cx| {
            let PluginWindowEvent::Selected(view) = event;
            workbench.plugin_tool_windows.selected.insert(window_id.clone(), view.clone());
            workbench.ensure_plugin_view(&window_id, view, window, cx);
        });
        self.plugin_tool_windows.containers.insert(id.to_owned(), container.clone());
        let active = container.read(cx).active().map(str::to_owned);
        if let Some(active) = active {
            self.ensure_plugin_view(id, &active, window, cx);
        }
        Some(Panel::new(container, Some(events)))
    }

    /// Gives the tab of `view` in mounted window `window_id` its panel, and asks the plugin to render
    /// it; a web page view says it cannot be drawn here.
    fn ensure_plugin_view(&mut self, window_id: &str, view: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(container) = self.plugin_tool_windows.containers.get(window_id).cloned() else { return };
        let Some(decl) = self
            .plugin_tool_windows
            .windows
            .get(window_id)
            .and_then(|state| state.spec.members.iter().find(|member| member.id == view).cloned())
        else {
            return;
        };
        let key: ViewKey = (decl.plugin_id.clone(), decl.id.clone());
        if let Some(slot) = self.plugin_tool_windows.views.get(&key) {
            let panel = slot.panel.clone();
            container.update(cx, |container, cx| container.set_panel(view, panel, cx));
            return;
        }
        let panel = cx.new(|cx| PluginViewPanel::new(&decl.plugin_id, &decl.id, cx));
        let events = cx.subscribe_in(&panel, window, {
            let key = key.clone();
            move |workbench, _, event: &PluginViewEvent, window, cx| {
                workbench.plugin_view_event(&key, event, window, cx)
            }
        });
        let web = decl.web;
        self.plugin_tool_windows.views.insert(
            key.clone(),
            ViewSlot {
                window: window_id.to_owned(),
                panel: panel.clone(),
                queue: VecDeque::new(),
                sending: false,
                _events: events,
            },
        );
        container.update(cx, |container, cx| container.set_panel(view, panel.clone(), cx));
        if web {
            panel.update(cx, |panel, cx| {
                let (seq, _) = panel.begin_render();
                let page = View { toolbar: Vec::new(), body: ViewBody::Html(String::new()), status: None };
                panel.finish_render(seq, Ok(page), cx);
            });
        } else {
            self.render_plugin_view(&key, cx);
        }
    }

    /// Whether the desktop command `command` may run for this window now.
    fn plugin_view_allowed(&self, command: &str, cx: &App) -> IdeResult<()> {
        match cx.try_global::<Backend>() {
            Some(backend) => backend.host.authorize_command(&self.label, command),
            None => Err(IdeError::internal("the backend is not running")),
        }
    }

    /// Asks the plugin for the content of view `key`, off the main thread; the panel drops an
    /// answer that a later render replaced.
    fn render_plugin_view(&mut self, key: &ViewKey, cx: &mut Context<Self>) {
        let Some(panel) = self.plugin_tool_windows.views.get(key).map(|slot| slot.panel.clone()) else { return };
        let (seq, viewport) = panel.update(cx, |panel, _| panel.begin_render());
        let allowed = self.plugin_view_allowed("plugin_view_render", cx);
        let (host, projects) = (plugin_host(cx), cx.try_global::<Backend>().map(|backend| backend.host.clone()));
        let (editor, label) = (self.plugin_editor(cx), self.label.clone());
        let (plugin, view) = key.clone();
        let rendered = crate::plugins::host_blocking(cx, move || {
            allowed?;
            let plugins = host.ok_or_else(|| IdeError::internal("the plugin host is not running"))?;
            let projects = projects.ok_or_else(|| IdeError::internal("the backend is not running"))?;
            let invocation = ide_host::plugins::views::view_invocation(&projects, &label, None, editor)?;
            plugins.render_view(&plugin, &view, viewport, invocation)
        });
        let panel = panel.downgrade();
        cx.spawn(async move |_, cx| {
            let result = rendered.await.map_err(|error| error.message);
            let _ = panel.update(cx, |panel, cx| panel.finish_render(seq, result, cx));
        })
        .detach();
    }

    /// Queues `event` for the plugin's `handle` of view `key`.
    fn queue_plugin_view_event(&mut self, key: &ViewKey, event: ViewEvent, cx: &mut Context<Self>) {
        let Some(slot) = self.plugin_tool_windows.views.get_mut(key) else { return };
        slot.queue.push_back(event);
        self.send_next_plugin_view_event(key, cx);
    }

    /// Delivers the next queued event of view `key` once the one before it was handled; a failure
    /// shows above the view.
    fn send_next_plugin_view_event(&mut self, key: &ViewKey, cx: &mut Context<Self>) {
        let allowed = self.plugin_view_allowed("plugin_view_event", cx);
        let (host, projects) = (plugin_host(cx), cx.try_global::<Backend>().map(|backend| backend.host.clone()));
        let (editor, label) = (self.plugin_editor(cx), self.label.clone());
        let Some(slot) = self.plugin_tool_windows.views.get_mut(key) else { return };
        if slot.sending {
            return;
        }
        let Some(event) = slot.queue.pop_front() else { return };
        slot.sending = true;
        let panel = slot.panel.downgrade();
        let (plugin, view) = key.clone();
        let handled = crate::plugins::host_blocking(cx, move || {
            allowed?;
            let plugins = host.ok_or_else(|| IdeError::internal("the plugin host is not running"))?;
            let projects = projects.ok_or_else(|| IdeError::internal("the backend is not running"))?;
            let invocation = ide_host::plugins::views::view_invocation(&projects, &label, None, editor)?;
            plugins.view_event(&plugin, &view, event, invocation)
        });
        let key = key.clone();
        cx.spawn(async move |workbench, cx| {
            let result = handled.await;
            let _ = workbench.update(cx, |workbench, cx| {
                if let Err(error) = result {
                    let _ = panel.update(cx, |panel, cx| panel.set_event_error(error.message, cx));
                }
                if let Some(slot) = workbench.plugin_tool_windows.views.get_mut(&key) {
                    slot.sending = false;
                }
                workbench.send_next_plugin_view_event(&key, cx);
            });
        })
        .detach();
    }

    /// Carries out what a view's panel asks.
    fn plugin_view_event(
        &mut self,
        key: &ViewKey,
        event: &PluginViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            PluginViewEvent::Render { .. } => self.render_plugin_view(key, cx),
            PluginViewEvent::Send(event) => self.queue_plugin_view_event(key, event, cx),
            PluginViewEvent::ItemMenu { key: item, position, table } => {
                self.show_plugin_item_menu(key, &item, position, table, window, cx);
            }
        }
    }

    /// The menu of item `item` of view `key`: the plugin's `viewItem.context` entries whose `when`
    /// holds with `viewItem` and `viewItemKind` (the item key up to its first `:`) set, each running
    /// its command with the item key, then an editable table's row commands. Nothing opens without
    /// an entry.
    fn show_plugin_item_menu(
        &mut self,
        key: &ViewKey,
        item: &str,
        position: Point<Pixels>,
        table: Option<TableMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut ctx = (*window_context(window, cx)).clone();
        ctx.set("viewItem", item);
        ctx.set("viewItemKind", item.split(':').next().unwrap_or_default());
        let mut items: Vec<MenuItem<TableRowCommand>> = view_item_menu(&key.0, &key.1, item, &ctx, cx);
        if let Some(table) = table.filter(|table| table.editable) {
            if !items.is_empty() {
                items.push(MenuItem::Separator);
            }
            items.extend(table_row_menu(table.row, &table.rows, cx));
        }
        if items.is_empty() {
            return;
        }
        let key = key.clone();
        let run = move |workbench: &mut Self, command: TableRowCommand, _: &mut Window, cx: &mut Context<Self>| {
            workbench.queue_plugin_view_event(&key, command.event(), cx);
        };
        self.show_menu(&items, position, run, window, cx);
    }

    // ---------------------------------------------------------------------------------------------
    // What plugin code asks

    /// Plugin code asked for view `view` to be rendered again: this window's panel of it is.
    pub(super) fn plugin_view_changed(&mut self, plugin_id: &str, view: &str, cx: &mut Context<Self>) {
        let key: ViewKey = (plugin_id.to_owned(), view.to_owned());
        if self.plugin_tool_windows.views.contains_key(&key) {
            self.render_plugin_view(&key, cx);
        }
    }

    /// Plugin code asked to bring view `view` to the front of this window: its window shows with
    /// the keyboard, its tab chosen. A view its `when` hides, or no longer declared, stays hidden.
    pub(super) fn reveal_plugin_view(
        &mut self,
        plugin_id: &str,
        view: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self
            .plugin_tool_windows
            .windows
            .iter()
            .find(|(_, state)| {
                state.spec.plugin_id == plugin_id && state.members.iter().any(|(member, _)| member == view)
            })
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        self.plugin_tool_windows.selected.insert(id.clone(), view.to_owned());
        if let Some(container) = self.plugin_tool_windows.containers.get(&id).cloned() {
            container.update(cx, |container, cx| container.select(view, cx));
            self.ensure_plugin_view(&id, view, window, cx);
        }
        self.with_layout(window, cx, |layout| layout.show(&id, true));
    }

    /// Plugin code posted a message to view `view`'s web page: this window draws none, so nobody
    /// receives it. Only the names are logged: the message is the plugin's data.
    pub(super) fn plugin_view_message(&self, plugin_id: &str, view: &str) {
        tracing::debug!("plugin {plugin_id} posted to the page of its view {view}, which this window does not draw");
    }

    // ---------------------------------------------------------------------------------------------
    // Read by tests

    /// The panel of plugin `plugin_id`'s view `view`, once its tab was shown.
    pub fn plugin_view_panel(&self, plugin_id: &str, view: &str) -> Option<Entity<PluginViewPanel>> {
        self.plugin_tool_windows.views.get(&(plugin_id.to_owned(), view.to_owned())).map(|slot| slot.panel.clone())
    }

    /// The view of plugin window `id`, once it was shown.
    pub fn plugin_tool_window(&self, id: &str) -> Option<Entity<PluginToolWindowView>> {
        self.plugin_tool_windows.containers.get(id).cloned()
    }
}

/// Registers `toolwindow.<id>` for every plugin window `(id, title, icon)` and withdraws the ones
/// of windows no longer declared; it shows, focuses or hides the window as the IDE's own do.
fn sync_window_actions(windows: &[(String, String, String)], cx: &mut App) {
    let wanted: BTreeMap<String, (String, String)> = windows
        .iter()
        .map(|(id, title, icon)| (format!("{TOOL_WINDOW_ACTION_PREFIX}{id}"), (title.clone(), icon.clone())))
        .collect();
    let previous = std::mem::take(&mut cx.default_global::<PluginWindowActions>().0);
    let registry = cx.default_global::<ActionRegistry>();
    for (id, shown) in &previous {
        if wanted.get(id) != Some(shown) {
            registry.unregister(id);
        }
    }
    for (id, (title, icon)) in &wanted {
        if previous.get(id) == Some(&(title.clone(), icon.clone())) {
            continue;
        }
        let target = id[TOOL_WINDOW_ACTION_PREFIX.len()..].to_owned();
        let meta = ActionMeta {
            category: Some("Tool Windows".to_owned()),
            icon: Some(icon.clone()),
            keywords: ["tool window", "show", "hide"].map(str::to_owned).to_vec(),
            verbatim: true,
            ..ActionMeta::new(id, title)
        };
        registry.register(Action::new(meta, move |_, window, cx| {
            let target = target.clone();
            on_workbench(window, cx, move |workbench, window, cx| {
                workbench.with_layout(window, cx, |layout| layout.activate(&target));
            })
        }));
    }
    cx.default_global::<PluginWindowActions>().0 = wanted;
}

/// The `viewItem.context` entries of plugin `plugin`'s view `view` for item `item`, as `ctx`
/// judges their `when`: each runs its command with the item key; a plugin's entries naming one
/// submenu are grouped under it, a group of one is that entry. Unregistered commands are left out.
fn view_item_menu<C: Clone>(plugin: &str, view: &str, item: &str, ctx: &ContextKeys, cx: &App) -> Vec<MenuItem<C>> {
    let registry = cx.try_global::<ActionRegistry>();
    let mut out: Vec<MenuItem<C>> = Vec::new();
    let mut groups: HashMap<String, usize> = HashMap::new();
    let listed = listed_contributions(crate::plugins::context_listing(ctx, cx));
    let entries = listed.iter().filter(|c| {
        c.point == MENUS_POINT
            && c.plugin_id == plugin
            && c.text("menu") == Some(VIEW_ITEM_MENU)
            && c.text("view") == Some(view)
    });
    for c in entries {
        if !evaluate_when(c.when(), ctx).unwrap_or(false) {
            continue;
        }
        let Some(command) = c.text("command") else { continue };
        let action = plugin_action_id(plugin, command);
        if !registry.is_some_and(|registry| registry.contains(&action)) {
            continue;
        }
        let entry = MenuItem::action_with(
            super::plugin_contributions::static_action_id(&action),
            Value::String(item.to_owned()),
        );
        match c.text("submenu").filter(|submenu| !submenu.is_empty()) {
            None => out.push(entry),
            Some(submenu) => {
                let at = *groups.entry(submenu.to_owned()).or_insert_with(|| {
                    out.push(MenuItem::submenu(submenu, Vec::new()));
                    out.len() - 1
                });
                if let MenuItem::Item(group) = &mut out[at] {
                    group.submenu.push(entry);
                }
            }
        }
    }
    out.into_iter()
        .map(|item| match item {
            MenuItem::Item(MenuEntry { mut submenu, .. }) if submenu.len() == 1 => submenu.remove(0),
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::plugins_window::test_support::{link, plugin_copy, start_with_plugins, trusted_project};
    use super::super::tests::{click_on, notice_titled, run_action, wait_long};
    use super::*;
    use crate::platform::context::ContextValue;
    use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, TestAppContext, VisualTestContext};
    use ide_host::plugins::PluginStatus;
    use serde_json::json;

    mod proofs9_view_icon;

    const LABEL: &str = "w1";
    const GREETINGS: &str = "plugin.hello.greetings";

    fn view(plugin: &str, decl: Value) -> Contribution {
        let key = decl["id"].as_str().unwrap_or_default().to_owned();
        Contribution::new(plugin, VIEWS_POINT, &key, decl)
    }

    // src/features/plugins/__tests__/viewWindows.test.tsx
    #[test]
    fn keeps_other_plugins_and_ungrouped_views_separate_even_when_their_ids_equal_a_group() {
        let specs = window_specs(&[
            view("spring", json!({ "id": "routes", "title": "Routes", "location": "left", "group": "framework" })),
            view("other", json!({ "id": "routes", "title": "Routes", "group": "framework" })),
            view("spring", json!({ "id": "framework", "title": "Framework" })),
            view("spring", json!({ "id": "beans", "title": "Beans", "group": "framework" })),
            view("spring", json!({ "id": "svc", "title": "Svc", "location": "services" })),
        ]);
        let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids, ["plugin.spring.group:framework", "plugin.other.group:framework", "plugin.spring.framework"]);
        let members: Vec<&str> = specs[0].members.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(members, ["routes", "beans"], "declaration order");
        assert_eq!((specs[0].first().title.as_str(), specs[0].first().anchor), ("Routes", Anchor::Left));
    }

    // src/features/plugins/__tests__/viewWindows.test.tsx
    #[test]
    fn uses_the_first_group_members_split_even_when_hidden_and_keeps_views_primary_unless_secondary_is_true() {
        let specs = window_specs(&[
            view(
                "spring",
                json!({ "id": "routes", "title": "r", "group": "g", "secondary": true, "when": "plugin.spring.ready" }),
            ),
            view("spring", json!({ "id": "beans", "title": "b", "group": "g", "secondary": false })),
            view("split", json!({ "id": "a", "title": "a" })),
            view("split", json!({ "id": "b", "title": "b", "secondary": false })),
        ]);
        assert!(specs[0].first().secondary);
        assert_eq!(specs[0].shown(&ContextKeys::new()).iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["beans"]);
        assert!(!specs[1].first().secondary && !specs[2].first().secondary);
    }

    // src/features/plugins/__tests__/viewWindows.test.tsx
    #[test]
    fn keeps_legacy_ungrouped_window_ids_and_removes_conditional_views_until_they_apply() {
        let specs = window_specs(&[
            view("legacy", json!({ "id": "always", "title": "Always" })),
            view("legacy", json!({ "id": "maybe", "title": "Maybe", "when": "plugin.legacy.ready" })),
            view("legacy", json!({ "id": "broken", "title": "Broken", "when": "ready && (" })),
        ]);
        let mut ctx = ContextKeys::new();
        let shown = |ctx: &ContextKeys| -> Vec<String> {
            specs.iter().filter(|spec| !spec.shown(ctx).is_empty()).map(|spec| spec.id.clone()).collect()
        };
        assert_eq!(shown(&ctx), ["plugin.legacy.always"]);
        ctx.set("plugin.legacy.ready", true);
        assert_eq!(shown(&ctx), ["plugin.legacy.always", "plugin.legacy.maybe"]);
    }

    fn context(key: &str, cx: &mut VisualTestContext) -> Option<ContextValue> {
        cx.update(|window, cx| window_context(window, cx).get(key).cloned())
    }

    fn registered(workbench: &Entity<Workbench>, id: &str, cx: &mut VisualTestContext) -> Option<ToolWindow> {
        workbench.read_with(cx, |w, _| w.tool_windows().and_then(|layout| layout.window(id).cloned()))
    }

    fn status_of(plugin: &str, cx: &mut VisualTestContext) -> Option<PluginStatus> {
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        host.list(None).plugins.into_iter().find(|p| p.id == plugin).map(|p| p.status)
    }

    /// The panel of `plugin`'s view `view` once it shows `lines`.
    fn wait_lines(workbench: &Entity<Workbench>, plugin: &str, view: &str, lines: &[&str], cx: &mut VisualTestContext) {
        wait_long(cx, "the view's lines", |cx| {
            let panel = workbench.read_with(cx, |w, _| w.plugin_view_panel(plugin, view));
            panel.is_some_and(|panel| panel.read_with(cx, |panel, _| panel.lines()) == lines)
        });
    }

    fn panel(
        workbench: &Entity<Workbench>,
        plugin: &str,
        view: &str,
        cx: &mut VisualTestContext,
    ) -> Entity<PluginViewPanel> {
        workbench.read_with(cx, |w, _| w.plugin_view_panel(plugin, view)).expect("the view has its panel")
    }

    fn double_click(cx: &mut VisualTestContext, selector: &'static str) {
        let position = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn")).center();
        for count in [1, 2] {
            let (modifiers, button) = (Modifiers::default(), MouseButton::Left);
            cx.simulate_event(MouseDownEvent { position, modifiers, button, click_count: count, first_mouse: false });
            cx.simulate_event(MouseUpEvent { position, modifiers, button, click_count: count });
        }
        cx.run_until_parked();
    }

    /// A linked copy of the Hello fixture, its manifest rewritten by `edit`, in a trusted project.
    fn hello(
        cx: &mut TestAppContext,
        edit: impl Fn(&str) -> String,
    ) -> (
        super::super::plugins_window::test_support::PluginHome,
        tempfile::TempDir,
        Entity<Workbench>,
        &mut VisualTestContext,
    ) {
        let home = start_with_plugins(cx);
        let folder = plugin_copy(home.path(), "hello", edit);
        link(cx, &folder, &[], "word-count");
        let (folder, _root, workbench, cx) = trusted_project(cx, LABEL, &[]);
        wait_long(cx, "the view's tool window", |cx| registered(&workbench, GREETINGS, cx).is_some());
        (home, folder, workbench, cx)
    }

    const LANGUAGES: [&str; 3] = ["▸ English", "▸ Deutsch", "▸ 日本語"];

    /// The Greetings view of the Hello fixture is a tool window titled and drawn as declared, on its
    /// stripe, before the plugin runs; opened by its action it shows the tree the plugin renders,
    /// with its status line and toolbar, and has the keyboard (`activeToolWindow`); the action
    /// again hides it and the key goes.
    #[gpui_kit::test]
    async fn a_plugins_view_is_a_tool_window_with_its_title_and_icon_that_shows_the_rendered_rows(
        cx: &mut TestAppContext,
    ) {
        let (_home, _folder, workbench, cx) = hello(cx, str::to_owned);
        let declared = registered(&workbench, GREETINGS, cx).expect("registered");
        assert_eq!(
            (declared.title.as_str(), declared.icon.as_str(), declared.anchor),
            ("Greetings", "plugin", Anchor::Right)
        );
        assert!(declared.verbatim, "the plugin's title is shown as written");
        assert!(cx.debug_bounds("stripe:plugin.hello.greetings").is_some());
        assert_eq!(status_of("hello", cx), Some(PluginStatus::Inactive), "declaring a view runs no plugin code");

        run_action(cx, "toolwindow.plugin.hello.greetings", None);
        wait_lines(&workbench, "hello", "greetings", &LANGUAGES, cx);
        let shown = panel(&workbench, "hello", "greetings", cx);
        assert_eq!(shown.read_with(cx, |p, _| p.status().map(str::to_owned)).as_deref(), Some("3 languages"));
        let toolbar = shown.read_with(cx, |p, _| p.toolbar().to_vec());
        assert_eq!((toolbar[0].id.as_str(), toolbar[0].enabled), ("collapse", false));
        wait_long(cx, "the window has the keyboard", |cx| {
            context("activeToolWindow", cx) == Some(ContextValue::from(GREETINGS))
        });
        assert_eq!(context("toolWindow.plugin.hello.greetings.visible", cx), Some(ContextValue::Bool(true)));

        run_action(cx, "toolwindow.plugin.hello.greetings", None);
        wait_long(cx, "the window is hidden", |cx| context("activeToolWindow", cx).is_none());
        assert!(!workbench.read_with(cx, |w, _| w.is_tool_window_visible(GREETINGS)));
    }

    /// Opening a language reaches the plugin, which asks for a refresh: the window renders again
    /// and shows its greetings; a double click on a greeting activates it (the plugin's
    /// notification); Collapse All, now enabled, closes them again.
    #[gpui_kit::test]
    async fn clicking_a_node_reaches_the_plugin_and_its_refresh_repaints_the_view(cx: &mut TestAppContext) {
        let (_home, _folder, workbench, cx) = hello(cx, str::to_owned);
        run_action(cx, "toolwindow.plugin.hello.greetings", None);
        wait_lines(&workbench, "hello", "greetings", &LANGUAGES, cx);
        click_on(cx, "plugin-view-toggle:0");
        let open = ["▾ English", "  Hello", "  Good morning", "  Hi", "▸ Deutsch", "▸ 日本語"];
        wait_lines(&workbench, "hello", "greetings", &open, cx);
        double_click(cx, "plugin-view-row:3");
        wait_long(cx, "the plugin's notification", |cx| notice_titled(&workbench, "Hi", cx).is_some());
        click_on(cx, "plugin-view-button:collapse");
        wait_lines(&workbench, "hello", "greetings", &LANGUAGES, cx);
    }

    /// A view whose render the plugin fails shows why, in place of its content.
    #[gpui_kit::test]
    async fn a_view_that_fails_to_render_says_the_plugin_could_not_show_it(cx: &mut TestAppContext) {
        let broken = |text: &str| {
            format!("{text}\n[[contributes.views]]\nid = \"broken\"\ntitle = \"Broken\"\nlocation = \"bottom\"\n")
        };
        let (_home, _folder, workbench, cx) = hello(cx, broken);
        // The plugin's code is loaded through its own view first.
        run_action(cx, "toolwindow.plugin.hello.greetings", None);
        wait_lines(&workbench, "hello", "greetings", &LANGUAGES, cx);
        run_action(cx, "toolwindow.plugin.hello.broken", None);
        wait_long(cx, "the failure", |cx| {
            let panel = workbench.read_with(cx, |w, _| w.plugin_view_panel("hello", "broken"));
            panel.is_some_and(|panel| panel.read_with(cx, |p, _| p.error().is_some()))
        });
        let failed = panel(&workbench, "hello", "broken", cx);
        assert!(failed.read_with(cx, |p, _| p.content().is_none()));
        assert!(failed.read_with(cx, |p, _| p.error().is_some_and(|error| error.contains("broken"))));
        assert!(cx.debug_bounds("plugin-view-failed").is_some());
    }

    /// Disabling the plugin takes its window away: no stripe button, no panel, no action, no
    /// visibility key. Closing the project drops the plugin windows of the next one too.
    #[gpui_kit::test]
    async fn disabling_the_plugin_removes_its_tool_window(cx: &mut TestAppContext) {
        let (_home, _folder, workbench, cx) = hello(cx, str::to_owned);
        run_action(cx, "toolwindow.plugin.hello.greetings", None);
        wait_lines(&workbench, "hello", "greetings", &LANGUAGES, cx);
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        host.set_enabled("hello", false).expect("the plugin is disabled");
        cx.update(|_, cx| Plugins::refresh(cx));
        wait_long(cx, "the window goes", |cx| registered(&workbench, GREETINGS, cx).is_none());
        assert!(workbench.read_with(cx, |w, _| w.plugin_view_panel("hello", "greetings").is_none()));
        assert!(workbench.read_with(cx, |w, _| w.tool_window_view(GREETINGS).is_none()));
        assert!(cx.debug_bounds("stripe:plugin.hello.greetings").is_none());
        assert_eq!(context("toolWindow.plugin.hello.greetings.visible", cx), None);
        assert!(!cx.update(|_, cx| cx.global::<ActionRegistry>().contains("toolwindow.plugin.hello.greetings")));
    }

    #[gpui_kit::test]
    async fn closing_the_project_drops_its_plugin_windows(cx: &mut TestAppContext) {
        let (_home, _folder, workbench, cx) = hello(cx, str::to_owned);
        run_action(cx, "toolwindow.plugin.hello.greetings", None);
        wait_lines(&workbench, "hello", "greetings", &LANGUAGES, cx);
        workbench.update_in(cx, |w, window, cx| w.close_project(window, cx));
        cx.run_until_parked();
        assert!(workbench.read_with(cx, |w, _| w.plugin_view_panel("hello", "greetings").is_none()));
        assert!(workbench.read_with(cx, |w, _| w.plugin_tool_window(GREETINGS).is_none()));
    }

    /// A project in Restricted Mode does not load its own plugins (`.openidea/plugins`), so the
    /// views such a plugin declares get no tool window, no stripe button and no action, while an
    /// installed plugin's view shows as always.
    #[gpui_kit::test]
    async fn a_restricted_projects_own_plugin_shows_no_view(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let installed = plugin_copy(home.path(), "grouped-views", str::to_owned);
        link(cx, &installed, &[], "reveal");
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        let own = plugin_copy(home.path(), "hello", str::to_owned);
        super::super::plugins_window::test_support::copy_dir(&own, &root.join(".openidea/plugins/hello"));
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, false, false)).expect("previewed");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new(LABEL, window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        wait_long(cx, "the installed plugin's view", |cx| {
            registered(&workbench, "plugin.grouped-views.legacy", cx).is_some()
        });
        assert!(workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        assert!(!host.list(Some(&root)).plugins.iter().any(|p| p.id == "hello"), "the plugin is not loaded");
        assert!(registered(&workbench, GREETINGS, cx).is_none());
        assert!(cx.debug_bounds("stripe:plugin.hello.greetings").is_none());
        assert!(!cx.update(|_, cx| cx.global::<ActionRegistry>().contains("toolwindow.plugin.hello.greetings")));
        cx.update(|_, cx| cx.global::<Backend>().host.close_project(LABEL));
    }

    /// The Tables fixture: its sortable header asks the plugin to sort and the rows come back in
    /// its order; a row's selection reaches it (its status line says so); a row's menu offers the
    /// plugin's `viewItem.context` command, which runs with the row and refreshes the view.
    #[gpui_kit::test]
    async fn a_plugin_table_sorts_selects_and_offers_its_row_command(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let folder = plugin_copy(home.path(), "tables", str::to_owned);
        link(cx, &folder, &[], "inspect");
        let (_folder, _root, workbench, cx) = trusted_project(cx, LABEL, &[]);
        wait_long(cx, "the window", |cx| registered(&workbench, "plugin.tables.tables", cx).is_some());
        run_action(cx, "toolwindow.plugin.tables.tables", None);
        wait_lines(
            &workbench,
            "tables",
            "tables",
            &["ext | files", "1: rs | 20", "2: md | 3", "3: java | 9", "4: toml | 12"],
            cx,
        );
        click_on(cx, "plugin-view-header:1");
        wait_lines(
            &workbench,
            "tables",
            "tables",
            &["ext | files", "1: md | 3", "2: java | 9", "3: toml | 12", "4: rs | 20"],
            cx,
        );
        let shown = panel(&workbench, "tables", "tables", cx);
        let status = |cx: &mut VisualTestContext| shown.read_with(cx, |p, _| p.status().map(str::to_owned));
        click_on(cx, "plugin-view-table-row:1");
        wait_long(cx, "the selection reaches the plugin", |cx| status(cx).as_deref() == Some("select:1"));

        let row = cx.debug_bounds("plugin-view-table-row:0").expect("the row is drawn").center();
        cx.simulate_mouse_down(row, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(row, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        let menu = workbench.read_with(cx, |w, _| w.open_menu::<TableRowCommand>()).expect("the row's menu");
        let lines = menu.read_with(cx, |menu, _| menu.lines()[0].clone());
        let at =
            lines.iter().position(|line| line.trim_start_matches("> ") == "Inspect Row").expect("the plugin's entry");
        click_on(cx, Box::leak(format!("menu-row-0-{at}").into_boxed_str()));
        wait_long(cx, "the command ran with the row", |cx| status(cx).as_deref() == Some("context:0"));
    }

    /// Plugin code reveals its ungrouped view: the window shows with the keyboard. A group whose
    /// views' conditions do not hold has no window until one does; revealed then, it shows that
    /// view's tab.
    #[gpui_kit::test]
    async fn reveal_shows_and_focuses_the_views_window_and_a_hidden_view_stays_hidden(cx: &mut TestAppContext) {
        const LEGACY: &str = "plugin.grouped-views.legacy";
        const GROUP: &str = "plugin.grouped-views.group:demo";
        let home = start_with_plugins(cx);
        let folder = plugin_copy(home.path(), "grouped-views", str::to_owned);
        link(cx, &folder, &[], "reveal");
        let (_folder, _root, workbench, cx) = trusted_project(cx, LABEL, &[]);
        wait_long(cx, "the ungrouped window", |cx| registered(&workbench, LEGACY, cx).is_some());
        assert!(registered(&workbench, GROUP, cx).is_none(), "no view of the group is shown");
        let reveal = plugin_action_id("grouped-views", "reveal");
        run_action(cx, &reveal, Some(Value::from("overview")));
        cx.run_until_parked();
        assert!(!workbench.read_with(cx, |w, _| w.is_tool_window_visible(GROUP)), "a hidden view stays hidden");

        run_action(cx, &reveal, Some(Value::from("legacy")));
        wait_long(cx, "the window shows with the keyboard", |cx| {
            context("activeToolWindow", cx) == Some(ContextValue::from(LEGACY))
        });
        wait_lines(&workbench, "grouped-views", "legacy", &["Ungrouped view keeps its original window id"], cx);

        run_action(cx, &plugin_action_id("grouped-views", "show"), None);
        wait_long(cx, "the group's window", |cx| registered(&workbench, GROUP, cx).is_some());
        assert_eq!(registered(&workbench, GROUP, cx).map(|w| w.title), Some("Grouped Views".to_owned()));
        run_action(cx, &reveal, Some(Value::from("overview")));
        wait_lines(&workbench, "grouped-views", "overview", &["Context condition is true"], cx);
        assert!(workbench.read_with(cx, |w, _| w.is_tool_window_visible(GROUP)));
        let group = workbench.read_with(cx, |w, _| w.plugin_tool_window(GROUP)).expect("mounted");
        let tabs: Vec<String> = group.read_with(cx, |g, _| g.members().iter().map(|(id, _)| id.clone()).collect());
        assert_eq!(tabs, ["overview"], "the view of a missing file is not shown");

        run_action(cx, &plugin_action_id("grouped-views", "hide"), None);
        wait_long(cx, "the group's window goes", |cx| registered(&workbench, GROUP, cx).is_none());
    }
}
