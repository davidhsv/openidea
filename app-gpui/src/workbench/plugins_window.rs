//! The plugin pages of a window carried out on the plugin host: Settings › Plugins, the Welcome
//! screen's Plugins page, and the page the Plugins actions drive while neither is open. Every
//! operation a page asks for runs on the host off the main thread and is answered to the page
//! that asked; the dialogs they need (the install and permission page, the repository prompt)
//! show in a layer of their own over everything else, Settings included.
//!
//! The window's plugin host is the app's ([`Plugins`]) unless one is handed to it
//! ([`Workbench::use_plugin_host`]); the pages follow the host's announcements (the plugins
//! changed, a plugin asks for permission) through the app's listener.
//!
//! Secrets typed into a page reach the host's secret store and nothing else: no log line names a
//! value or an event's fields, only the event's kind.

use super::Workbench;
use super::notifications::{NotificationLevel, NotificationRequest};
use super::services_window::on_workbench;
use crate::backend::Backend;
use crate::platform::actions::{ActionRegistry, ActionSource, execute_action_from};
use crate::platform::i18n::{Localization, Param};
use crate::platform::settings::{SettingsWrite, global, set_global};
use crate::plugin_models::consent::Capability;
use crate::plugin_models::contribution::Contribution;
use crate::plugin_models::plugins_page::{
    ConsentDialog, ConsentDialogEvent, PluginsPage, PluginsPageEvent, repository_url,
};
use crate::plugin_models::store::{ConsentRequest, InstallStep, grant_request};
use crate::plugins::{Plugins, plugin_action_id};
use crate::vcs::prompt_dialogs::{FieldRule, PromptDialog, PromptDialogEvent};
use crate::vcs_ops::dialogs::PromptRequest;
use gpui_kit::{
    AnyElement, AnyView, App, Context, Entity, FocusHandle, Focusable, IntoElement, PathPromptOptions, Subscription,
    Task, WeakEntity, Window, div, prelude::*,
};
use ide_core::IdeResult;
use ide_host::plugins::{Consent, PluginEvent, PluginHost};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How long a listing of the plugins waits for the installed ones to have loaded.
const INITIAL_LOAD_WAIT: Duration = Duration::from_secs(5);

/// The page a window's plugin pages and the Plugins actions work on, with the host it answers to.
struct HostedPage {
    page: Entity<PluginsPage>,
    _events: Subscription,
}

/// A dialog of the plugin pages, over everything else in the window.
enum DialogView {
    Consent(Entity<ConsentDialog>),
    Repository(Entity<PromptDialog>),
}

impl DialogView {
    fn view(&self) -> AnyView {
        match self {
            DialogView::Consent(dialog) => dialog.clone().into(),
            DialogView::Repository(dialog) => dialog.clone().into(),
        }
    }
}

struct PluginsDialog {
    view: DialogView,
    /// What had the keyboard before the dialog opened.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

/// Who a consent dialog answers: the page whose grant or install asked, or the host, for a plugin
/// that asked for permission while it ran.
enum ConsentFor {
    Page(WeakEntity<PluginsPage>),
    Host { host: Arc<PluginHost>, plugin_id: String, name: String },
}

/// What a window keeps for its plugin pages.
#[derive(Default)]
pub(super) struct PluginsSlot {
    /// The window's plugin host; none: the app's, if it has one.
    host: Option<Arc<PluginHost>>,
    /// The Welcome screen's Plugins page, while the Welcome screen shows.
    welcome: Option<HostedPage>,
    /// The page the Plugins actions drive while no other page is open; it is never drawn.
    background: Option<HostedPage>,
    dialog: Option<PluginsDialog>,
    /// The window's listener on the app's plugin host.
    listener: Option<u64>,
}

/// The app's plugin host, when it runs one.
fn app_plugin_host(cx: &App) -> Option<Arc<PluginHost>> {
    cx.try_global::<Plugins>().map(|plugins| Arc::clone(&plugins.host))
}

/// Where the local catalog of plugins is read from: the repository's official plugins in debug
/// builds; a released build has none next to it.
fn catalog_dirs() -> Vec<PathBuf> {
    if cfg!(debug_assertions) {
        vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/official")]
    } else {
        Vec::new()
    }
}

/// Runs `work` on `host` off the main thread; the failure is its message.
fn host_work<T, W>(host: &Arc<PluginHost>, cx: &App, work: W) -> Task<Result<T, String>>
where
    T: Send + 'static,
    W: FnOnce(&PluginHost) -> IdeResult<T> + Send + 'static,
{
    let host = Arc::clone(host);
    cx.background_executor().spawn(async move { work(&host).map_err(|error| error.message) })
}

/// The name of a Plugins page event, for the log: never its fields, which may hold a secret.
fn event_name(event: &PluginsPageEvent) -> &'static str {
    match event {
        PluginsPageEvent::Refresh { .. } => "Refresh",
        PluginsPageEvent::SetEnabled { .. } => "SetEnabled",
        PluginsPageEvent::Reload(_) => "Reload",
        PluginsPageEvent::Uninstall(_) => "Uninstall",
        PluginsPageEvent::Grant { .. } => "Grant",
        PluginsPageEvent::SetCapability { .. } => "SetCapability",
        PluginsPageEvent::RollBack(_) => "RollBack",
        PluginsPageEvent::AskConsent(_) => "AskConsent",
        PluginsPageEvent::Install(_) => "Install",
        PluginsPageEvent::DownloadMarket(_) => "DownloadMarket",
        PluginsPageEvent::EditRepository { .. } => "EditRepository",
        PluginsPageEvent::ImportInstalledIntellij => "ImportInstalledIntellij",
        PluginsPageEvent::ImportIntellijFile => "ImportIntellijFile",
        PluginsPageEvent::OpenUrl(_) => "OpenUrl",
        PluginsPageEvent::SetTheme(_) => "SetTheme",
        PluginsPageEvent::SelectIconTheme(_) => "SelectIconTheme",
        PluginsPageEvent::RunCommand { .. } => "RunCommand",
        PluginsPageEvent::LoadSecrets(_) => "LoadSecrets",
        PluginsPageEvent::SetSecret { .. } => "SetSecret",
        PluginsPageEvent::Notify(_) => "Notify",
    }
}

/// A path as the page carries it.
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The capabilities a plugin asked for, as the consent dialog reads them.
fn capabilities_from<T: serde::Serialize>(capabilities: &T) -> Vec<Capability> {
    serde_json::to_value(capabilities).ok().and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default()
}

/// Registers the Plugins actions that drive a page outside the open dialogs: Install Plugin from
/// Disk… (the argument is a folder, its `plugin.toml` or a package file; none: a picker) and Reload
/// Plugins. Neither has a condition.
pub(super) fn register_actions(registry: &mut ActionRegistry) {
    registry.handle("plugins.installFromDisk", |call, window, cx| {
        let path = call.arg.and_then(Value::as_str).map(str::to_owned);
        on_workbench(window, cx, move |workbench, window, cx| match workbench.action_plugins_page(window, cx) {
            Some(page) => page.update(cx, |page, cx| page.install_from_disk(path.as_deref(), false, true, cx)),
            None => tracing::info!("Plugins: this window has no plugin host to install with"),
        })
    });
    registry.handle("plugins.reloadAll", |_, window, cx| {
        on_workbench(window, cx, |workbench, _, cx| workbench.reload_all_plugins(cx))
    });
}

impl Workbench {
    /// The plugin host this window works with: the one handed to it, else the app's.
    pub fn plugin_host(&self, cx: &App) -> Option<Arc<PluginHost>> {
        self.plugin_ui.host.clone().or_else(|| app_plugin_host(cx))
    }

    /// Makes `host` the window's plugin host in place of the app's, and gives the Welcome screen
    /// its Plugins page over it. Windows of an app without a plugin host of its own (tests, a
    /// remote host's window) are handed one this way.
    pub fn use_plugin_host(&mut self, host: Arc<PluginHost>, window: &mut Window, cx: &mut Context<Self>) {
        self.plugin_ui.host = Some(host);
        self.plugin_ui.welcome = None;
        self.plugin_ui.background = None;
        self.sync_welcome_plugins(window, cx);
    }

    /// The Welcome screen's Plugins page, while the Welcome screen shows one.
    pub fn welcome_plugins_page(&self) -> Option<&Entity<PluginsPage>> {
        self.plugin_ui.welcome.as_ref().map(|hosted| &hosted.page)
    }

    /// The consent dialog of the plugin pages, while it is open.
    pub fn plugins_consent_dialog(&self) -> Option<&Entity<ConsentDialog>> {
        match &self.plugin_ui.dialog.as_ref()?.view {
            DialogView::Consent(dialog) => Some(dialog),
            DialogView::Repository(_) => None,
        }
    }

    /// The Plugin Repository prompt, while it is open.
    pub fn plugins_repository_prompt(&self) -> Option<&Entity<PromptDialog>> {
        match &self.plugin_ui.dialog.as_ref()?.view {
            DialogView::Repository(dialog) => Some(dialog),
            DialogView::Consent(_) => None,
        }
    }

    /// Listens to the app's plugin host for this window: the pages are listed again when the
    /// plugins change, and a plugin that asks for permission while it runs gets its consent dialog.
    /// Call once, when the window is made.
    pub(super) fn listen_to_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugin_ui.listener.is_some() || !cx.has_global::<Plugins>() {
            return;
        }
        let (weak, handle) = (cx.weak_entity(), window.window_handle());
        let listener = cx.global_mut::<Plugins>().listen(move |client, event, cx| {
            let _ = handle.update(cx, |_, window, cx| {
                weak.update(cx, |workbench, cx| workbench.plugin_host_event(client, event, window, cx))
            });
        });
        self.plugin_ui.listener = Some(listener);
    }

    /// The window goes: its listener on the plugin host goes with it.
    pub(super) fn stop_listening_to_plugins(&mut self, cx: &mut App) {
        if let Some(listener) = self.plugin_ui.listener.take()
            && cx.has_global::<Plugins>()
        {
            cx.global_mut::<Plugins>().unlisten(listener);
        }
    }

    /// Gives the Welcome screen its Plugins page while it shows and the window has a plugin host,
    /// and takes it away otherwise. The page is the same kind of view as Settings › Plugins, with
    /// its own listing.
    pub(super) fn sync_welcome_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(welcome) = self.welcome().cloned() else {
            self.plugin_ui.welcome = None;
            return;
        };
        if self.plugin_ui.welcome.is_some() {
            return;
        }
        let Some(host) = self.plugin_host(cx) else {
            welcome.update(cx, |view, cx| view.set_plugins_page(None, cx));
            return;
        };
        let (page, events) = self.new_plugins_page(host, window, cx);
        welcome.update(cx, |view, cx| view.set_plugins_page(Some(page.clone().into()), cx));
        self.plugin_ui.welcome = Some(HostedPage { page: page.clone(), _events: events });
        page.update(cx, |page, cx| page.opened(cx));
    }

    /// The Welcome screen went: its Plugins page goes with it.
    pub(super) fn drop_welcome_plugins(&mut self) {
        self.plugin_ui.welcome = None;
    }

    /// A Plugins page over `host` that this window carries out, following the window's project
    /// trust and the appearance settings. The caller shows it, then calls [`PluginsPage::opened`].
    pub(super) fn new_plugins_page(
        &mut self,
        host: Arc<PluginHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<PluginsPage>, Subscription) {
        let page = cx.new(|cx| PluginsPage::new(window, cx));
        let events = cx.subscribe_in(&page, window, move |workbench, page, event: &PluginsPageEvent, window, cx| {
            workbench.carry_out_plugins_event(&host, page, event, window, cx);
        });
        self.follow_plugins_settings(&page, cx);
        (page, events)
    }

    /// Every Plugins page of the window: Settings', the Welcome screen's and the actions' one.
    fn plugin_pages(&self) -> Vec<Entity<PluginsPage>> {
        let hosted = [&self.plugin_ui.welcome, &self.plugin_ui.background];
        self.settings_plugins_page()
            .cloned()
            .into_iter()
            .chain(hosted.into_iter().flatten().map(|hosted| hosted.page.clone()))
            .collect()
    }

    /// Tells the Plugins pages of the window the settings they follow (after a change of
    /// `appearance`, or of the project's trust).
    pub(super) fn follow_settings_on_plugin_pages(&self, cx: &mut Context<Self>) {
        for page in self.plugin_pages() {
            self.follow_plugins_settings(&page, cx);
        }
    }

    /// Tells a Plugins page whether the window's project is trusted, and the theme and the file
    /// icon theme the settings name.
    pub(super) fn follow_plugins_settings(&self, page: &Entity<PluginsPage>, cx: &mut Context<Self>) {
        let root = self.project_root(cx);
        let trusted = cx
            .try_global::<Backend>()
            .zip(root)
            .is_some_and(|(backend, root)| backend.host.trust.trusted(&root).unwrap_or(false));
        let appearance = global(cx).appearance;
        let icon_theme = Some(appearance.file_icon_theme).filter(|key| !key.is_empty());
        page.update(cx, |page, cx| {
            page.set_trusted_project(trusted, cx);
            page.set_appearance(Some(appearance.theme), icon_theme, cx);
        });
    }

    /// The page the Plugins actions drive: Settings' while it is open, else the Welcome screen's,
    /// else one made for them, listed once and kept (it is never drawn); none without a plugin host.
    fn action_plugins_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<PluginsPage>> {
        if let Some(page) = self.settings_plugins_page().or(self.welcome_plugins_page()) {
            return Some(page.clone());
        }
        if let Some(hosted) = &self.plugin_ui.background {
            return Some(hosted.page.clone());
        }
        let host = self.plugin_host(cx)?;
        let (page, events) = self.new_plugins_page(host, window, cx);
        self.plugin_ui.background = Some(HostedPage { page: page.clone(), _events: events });
        page.update(cx, |page, cx| page.opened(cx));
        Some(page)
    }

    /// Reload Plugins: every listed plugin is reloaded in turn; each failure is reported, and the
    /// pages list the plugins again.
    fn reload_all_plugins(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.plugin_host(cx) else {
            tracing::info!("Plugins: this window has no plugin host to reload");
            return;
        };
        let root = self.project_root(cx);
        let work = host_work(&host, cx, move |host| {
            let listed = host.list(root.as_deref());
            let failed = listed
                .plugins
                .into_iter()
                .filter_map(|plugin| host.reload(&plugin.id).err().map(|error| (plugin.name, error.message)))
                .collect::<Vec<_>>();
            Ok(failed)
        });
        cx.spawn(async move |workbench, cx| {
            let failed = work.await.unwrap_or_default();
            let _ = workbench.update(cx, |workbench, cx| {
                for (name, error) in failed {
                    let i18n = &mut cx.default_global::<Localization>().0;
                    let title = i18n.tx_built(&["Could not reload ", ""], &[Param::Text(name)]);
                    let request = NotificationRequest {
                        group: Some("plugins".to_owned()),
                        body: Some(error),
                        ..NotificationRequest::new(NotificationLevel::Error, title)
                    };
                    workbench.notify(request, cx);
                }
                workbench.list_plugins_on_pages(cx);
            });
        })
        .detach();
    }

    /// Lists the plugins again on every page of the window.
    fn list_plugins_on_pages(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.plugin_host(cx) else { return };
        for page in self.plugin_pages() {
            self.list_plugins(&host, &page, cx);
        }
    }

    /// What the plugin host announces for this window (`client`; none: every window).
    fn plugin_host_event(
        &mut self,
        client: Option<&str>,
        event: &PluginEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if client.is_some_and(|client| client != self.label) {
            return;
        }
        match event {
            PluginEvent::CommandsChanged => self.list_plugins_on_pages(cx),
            PluginEvent::ConsentRequested { plugin_id, capabilities } => {
                let Some(host) = self.plugin_host(cx) else { return };
                let listed = self.plugin_listing(cx);
                let plugin = listed.and_then(|listed| listed.plugins.iter().find(|plugin| &plugin.id == plugin_id));
                let mut request = match plugin {
                    Some(plugin) => grant_request(plugin),
                    None => ConsentRequest {
                        plugin_id: plugin_id.clone(),
                        name: plugin_id.clone(),
                        version: String::new(),
                        install: false,
                        capabilities: Vec::new(),
                        unsigned: false,
                        signer_changed: false,
                        warnings: Vec::new(),
                    },
                };
                request.capabilities = capabilities_from(capabilities);
                let answer = ConsentFor::Host { host, plugin_id: plugin_id.clone(), name: request.name.clone() };
                self.show_consent(request, answer, window, cx);
            }
            _ => {}
        }
    }

    /// Carries out what a Plugins page asks, on `host`, and answers the page.
    fn carry_out_plugins_event(
        &mut self,
        host: &Arc<PluginHost>,
        page: &Entity<PluginsPage>,
        event: &PluginsPageEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!("Plugins: {}", event_name(event));
        match event {
            PluginsPageEvent::Refresh { plugins, catalog, market } => {
                if *plugins {
                    self.list_plugins(host, page, cx);
                }
                if *catalog {
                    Self::read_plugin_catalog(host, page, cx);
                }
                if let Some(refresh) = *market {
                    Self::read_plugin_market(host, page, refresh, cx);
                }
            }
            PluginsPageEvent::SetEnabled { id, enabled } => {
                let (target, enabled) = (id.clone(), *enabled);
                let work = host_work(host, cx, move |host| host.set_enabled(&target, enabled));
                Self::answer_finished(page, id, work, cx);
            }
            PluginsPageEvent::Reload(id) => {
                let target = id.clone();
                let work = host_work(host, cx, move |host| host.reload(&target).map(|_| ()));
                Self::answer_finished(page, id, work, cx);
            }
            PluginsPageEvent::Uninstall(id) => {
                let target = id.clone();
                let work = host_work(host, cx, move |host| host.uninstall(&target));
                Self::answer_finished(page, id, work, cx);
            }
            PluginsPageEvent::Grant { id, optional, signer_change } => {
                let target = id.clone();
                let consent = Consent { optional: optional.clone(), signer_change: *signer_change };
                let work = host_work(host, cx, move |host| host.grant(&target, &consent).map(|_| ()));
                Self::answer_finished(page, id, work, cx);
            }
            PluginsPageEvent::SetCapability { id, capability, granted } => {
                let (target, capability, granted) = (id.clone(), capability.clone(), *granted);
                let work =
                    host_work(host, cx, move |host| host.set_capability(&target, &capability, granted).map(|_| ()));
                Self::answer_finished(page, id, work, cx);
            }
            PluginsPageEvent::RollBack(id) => {
                let (target, id) = (id.clone(), id.clone());
                let work = host_work(host, cx, move |host| host.roll_back_update(&target));
                Self::answer_page(page, work, move |page, result, cx| page.rolled_back(&id, result, cx), cx);
            }
            PluginsPageEvent::AskConsent(request) => {
                self.show_consent(request.clone(), ConsentFor::Page(page.downgrade()), window, cx);
            }
            PluginsPageEvent::Install(step) => self.install_step(host, page, step, window, cx),
            PluginsPageEvent::DownloadMarket(id) => {
                let (target, id) = (id.clone(), id.clone());
                let work =
                    host_work(host, cx, move |host| host.download_market_plugin(&target).map(|dir| path_text(&dir)));
                Self::answer_page(page, work, move |page, dir, cx| page.downloaded(&id, dir, cx), cx);
            }
            PluginsPageEvent::EditRepository { current } => {
                self.edit_plugin_repository(host, page, current.clone(), window, cx);
            }
            // Both imports convert a JetBrains theme into a plugin, which this app cannot do yet.
            PluginsPageEvent::ImportInstalledIntellij | PluginsPageEvent::ImportIntellijFile => {
                tracing::info!("Plugins: {} is not available in this window", event_name(event));
            }
            PluginsPageEvent::OpenUrl(url) => cx.open_url(url),
            // `plugins.rs` follows the theme setting into the host: only the setting is written.
            PluginsPageEvent::SetTheme(theme) => self.write_plugin_setting("appearance.theme", json!(theme), cx),
            PluginsPageEvent::SelectIconTheme(key) => {
                self.write_plugin_setting("appearance.fileIconTheme", json!(key.clone().unwrap_or_default()), cx);
            }
            PluginsPageEvent::RunCommand { plugin_id, command_id } => {
                let id = plugin_action_id(plugin_id, command_id);
                window.defer(cx, move |window, cx| {
                    execute_action_from(ActionSource::Other, &id, None, window, cx);
                });
            }
            PluginsPageEvent::LoadSecrets(id) => {
                let (target, id) = (id.clone(), id.clone());
                let work = host_work(host, cx, move |host| host.secrets(&target));
                Self::answer_page(
                    page,
                    work,
                    move |page, secrets, cx| match secrets {
                        Ok(secrets) => page.set_secrets(&id, secrets, cx),
                        // The state stays unknown: the page offers no stored value to remove.
                        Err(error) => tracing::warn!("Plugins: cannot read which secrets {id} has stored: {error}"),
                    },
                    cx,
                );
            }
            PluginsPageEvent::SetSecret { plugin_id, name, value } => {
                let (plugin, key, value) = (plugin_id.clone(), name.clone(), value.clone());
                let work = host_work(host, cx, move |host| host.set_secret(&plugin, &key, value.as_deref()));
                let (plugin_id, name) = (plugin_id.clone(), name.clone());
                let answer = move |page: &mut PluginsPage, saved: Result<(), String>, cx: &mut Context<PluginsPage>| {
                    page.secret_saved(&plugin_id, &name, saved, cx);
                };
                Self::answer_page(page, work, answer, cx);
            }
            PluginsPageEvent::Notify(request) => self.notify(request.clone(), cx),
        }
    }

    /// An install step: the picker, or unpacking, previewing and installing on the host.
    fn install_step(
        &mut self,
        host: &Arc<PluginHost>,
        page: &Entity<PluginsPage>,
        step: &InstallStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match step {
            InstallStep::Pick { package_file } => {
                let picked = cx.prompt_for_paths(PathPromptOptions {
                    files: *package_file,
                    directories: !*package_file,
                    multiple: false,
                    prompt: None,
                });
                let page = page.downgrade();
                cx.spawn_in(window, async move |_, cx| {
                    let picked = match picked.await {
                        Ok(Ok(paths)) => {
                            Ok(paths.and_then(|paths| paths.into_iter().next()).map(|path| path_text(&path)))
                        }
                        Ok(Err(error)) => Err(format!("{error:#}")),
                        // The picker went without answering: as cancelled.
                        Err(_) => Ok(None),
                    };
                    let _ = page.update(cx, |page, cx| page.picked(picked, cx));
                })
                .detach();
            }
            InstallStep::Unpack(file) => {
                let file = PathBuf::from(file);
                let work = host_work(host, cx, move |host| host.unpack_package(&file).map(|dir| path_text(&dir)));
                Self::answer_page(page, work, |page, dir, cx| page.unpacked(dir, cx), cx);
            }
            InstallStep::Preview(dir) => {
                let dir = PathBuf::from(dir);
                let work = host_work(host, cx, move |host| host.preview_install(&dir));
                Self::answer_page(page, work, |page, preview, cx| page.previewed(preview, cx), cx);
            }
            InstallStep::Install { dir, optional, signer_change } => {
                let dir = PathBuf::from(dir);
                let consent = Consent { optional: optional.clone(), signer_change: *signer_change };
                let work = host_work(host, cx, move |host| host.install_from_dir(&dir, &consent));
                Self::answer_page(page, work, |page, info, cx| page.installed(info, cx), cx);
            }
            // The page shows these itself.
            InstallStep::AskConsent(_) | InstallStep::Done { .. } => {}
        }
    }

    /// Writes a global setting the page chose; a failure is reported.
    fn write_plugin_setting(&mut self, key: &'static str, value: Value, cx: &mut Context<Self>) {
        let label = self.label.clone();
        if let Err(error) = set_global(&label, &[SettingsWrite { key, value }], cx) {
            let request = NotificationRequest {
                body: Some(error.message),
                key: Some(format!("plugins.setting:{key}")),
                ..NotificationRequest::new(NotificationLevel::Error, "Cannot save setting")
            };
            self.notify(request, cx);
        }
    }

    /// Lists the plugins as the window sees them: the installed and linked ones and the plugins of
    /// its project (loaded first when the project is trusted).
    fn list_plugins(&self, host: &Arc<PluginHost>, page: &Entity<PluginsPage>, cx: &mut Context<Self>) {
        let root = self.project_root(cx);
        // Only a trusted project's plugins are loaded. An untrusted one's were unloaded when it lost
        // its trust; asking the host to restrict it again would announce a change on every listing,
        // and every announced change lists the plugins again.
        let trusted = cx
            .try_global::<Backend>()
            .zip(root.as_ref())
            .is_some_and(|(backend, root)| backend.host.trust.trusted(root).unwrap_or(false));
        let work = host_work(host, cx, move |host| {
            host.wait_initial_load(INITIAL_LOAD_WAIT);
            host.sync_links();
            if let Some(root) = root.as_ref().filter(|_| trusted) {
                host.sync_project(root);
            }
            Ok(host.list(root.as_deref()))
        });
        Self::answer_page(
            page,
            work,
            |page, listed, cx| match listed {
                Ok(snapshot) => {
                    let contributions = Contribution::from_snapshot(&snapshot);
                    page.set_listing(snapshot.plugins, contributions, cx);
                }
                Err(error) => page.listing_failed(error, cx),
            },
            cx,
        );
    }

    fn read_plugin_catalog(host: &Arc<PluginHost>, page: &Entity<PluginsPage>, cx: &mut Context<Self>) {
        let dirs = catalog_dirs();
        let work = host_work(host, cx, move |host| Ok(host.catalog(&dirs)));
        Self::answer_page(page, work, |page, catalog, cx| page.set_catalog(catalog, cx), cx);
    }

    /// The marketplace's plugins, its index read again with `refresh`; none without an index.
    fn read_plugin_market(host: &Arc<PluginHost>, page: &Entity<PluginsPage>, refresh: bool, cx: &mut Context<Self>) {
        let work = host_work(host, cx, move |host| {
            let url = host.market_url();
            let listing = match url {
                Some(_) => host.market_listing(refresh).map_err(|error| error.message),
                None => Ok(Vec::new()),
            };
            Ok((url, listing))
        });
        Self::answer_page(
            page,
            work,
            |page, market, cx| match market {
                Ok((url, listing)) => page.set_market(url, listing, cx),
                Err(error) => page.set_market(None, Err(error), cx),
            },
            cx,
        );
    }

    /// Plugin Repository…: asks for the marketplace index URL, prefilled with `current`; an empty
    /// one restores the default. The marketplace is then read again from the new index.
    fn edit_plugin_repository(
        &mut self,
        host: &Arc<PluginHost>,
        page: &Entity<PluginsPage>,
        current: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.plugin_ui.dialog.is_some() {
            return;
        }
        let request = PromptRequest {
            title: "Plugin Repository".to_owned(),
            label: Some("Marketplace index URL:".to_owned()),
            initial: current.unwrap_or_default(),
            ok_label: "Save".to_owned(),
            mono: true,
            ..PromptRequest::default()
        };
        let dialog = cx.new(|cx| PromptDialog::prompt(request, FieldRule::Any, window, cx));
        let (host, page) = (Arc::clone(host), page.downgrade());
        let events = cx.subscribe_in(&dialog, window, move |workbench, _, event: &PromptDialogEvent, window, cx| {
            let PromptDialogEvent::Closed(answer) = event;
            workbench.close_plugins_dialog(window, cx);
            let (Some(result), Some(page)) = (answer.prompt_result(), page.upgrade()) else { return };
            let url = repository_url(&result.value);
            // Setting the URL cannot fail; the index it names is read again (and may fail) next.
            let work = host_work(&host, cx, move |host| {
                host.set_market_url(url);
                let url = host.market_url();
                let listing = match url {
                    Some(_) => host.market_listing(true).map_err(|error| error.message),
                    None => Ok(Vec::new()),
                };
                Ok((url, listing))
            });
            Self::answer_page(
                &page,
                work,
                |page, market, cx| match market {
                    Ok((url, listing)) => page.set_market(url, listing, cx),
                    Err(error) => page.set_market(None, Err(error), cx),
                },
                cx,
            );
        });
        let focus = dialog.focus_handle(cx);
        self.open_plugins_dialog(DialogView::Repository(dialog), focus, events, window, cx);
    }

    /// Shows the consent dialog for `request`; its answer goes to `answer`. While another dialog of
    /// the plugin pages is open, a page's request is answered as cancelled and the host's is left
    /// for the plugin to ask again.
    fn show_consent(
        &mut self,
        request: ConsentRequest,
        answer: ConsentFor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.plugin_ui.dialog.is_some() {
            if let ConsentFor::Page(page) = answer {
                let _ = page.update(cx, |page, cx| page.consented(None, cx));
            }
            return;
        }
        let dialog = cx.new(|cx| ConsentDialog::new(request, cx));
        let mut answer = Some(answer);
        let events = cx.subscribe_in(&dialog, window, move |workbench, _, event: &ConsentDialogEvent, window, cx| {
            workbench.close_plugins_dialog(window, cx);
            let consent = event.consent();
            match answer.take() {
                Some(ConsentFor::Page(page)) => {
                    // The page is gone when its dialog closed meanwhile.
                    let _ = page.update(cx, |page, cx| page.consented(consent, cx));
                }
                Some(ConsentFor::Host { host, plugin_id, name }) => {
                    if let Some(consent) = consent {
                        workbench.grant_asked(&host, plugin_id, name, consent, cx);
                    }
                }
                None => {}
            }
        });
        let focus = dialog.focus_handle(cx);
        self.open_plugins_dialog(DialogView::Consent(dialog), focus, events, window, cx);
    }

    /// The permission a running plugin asked for is granted as accepted; the pages list it again.
    fn grant_asked(
        &mut self,
        host: &Arc<PluginHost>,
        plugin_id: String,
        name: String,
        consent: Consent,
        cx: &mut Context<Self>,
    ) {
        let work = host_work(host, cx, move |host| host.grant(&plugin_id, &consent).map(|_| ()));
        cx.spawn(async move |workbench, cx| {
            let granted = work.await;
            let _ = workbench.update(cx, |workbench, cx| {
                if let Err(error) = granted {
                    let title = cx
                        .default_global::<Localization>()
                        .0
                        .tx_built(&["Could not enable ", ""], &[Param::Text(name)]);
                    let request = NotificationRequest {
                        group: Some("plugins".to_owned()),
                        body: Some(error),
                        ..NotificationRequest::new(NotificationLevel::Error, title)
                    };
                    workbench.notify(request, cx);
                }
                workbench.list_plugins_on_pages(cx);
            });
        })
        .detach();
    }

    fn open_plugins_dialog(
        &mut self,
        view: DialogView,
        focus: FocusHandle,
        events: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let restore = window.focused(cx);
        window.focus(&focus, cx);
        self.plugin_ui.dialog = Some(PluginsDialog { view, restore, _events: events });
        cx.notify();
    }

    /// Closes the dialog; the keyboard goes back to what had it (the page, mostly).
    fn close_plugins_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.plugin_ui.dialog.take() else { return };
        if let Some(target) = dialog.restore {
            window.focus(&target, cx);
        }
        cx.notify();
    }

    /// The dialog of the plugin pages over the window, Settings included, while one is open.
    pub(super) fn plugins_dialog_layer(&self) -> Option<AnyElement> {
        let view = self.plugin_ui.dialog.as_ref()?.view.view();
        let layer = div()
            .id("plugins-dialog-layer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .child(view);
        Some(gpui_kit::deferred(layer).with_priority(2).into_any_element())
    }

    /// Hands the answer to a page's `finished`, unless the page is gone by then.
    fn answer_finished(page: &Entity<PluginsPage>, id: &str, work: Task<Result<(), String>>, cx: &mut Context<Self>) {
        let id = id.to_owned();
        Self::answer_page(page, work, move |page, result, cx| page.finished(&id, result, cx), cx);
    }

    /// Hands the outcome of `work` to the page with `answer`, unless the page is gone by then.
    fn answer_page<T: 'static>(
        page: &Entity<PluginsPage>,
        work: Task<Result<T, String>>,
        answer: impl FnOnce(&mut PluginsPage, Result<T, String>, &mut Context<PluginsPage>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let page = page.downgrade();
        cx.spawn(async move |_, cx| {
            let outcome = work.await;
            // The page was closed meanwhile: nobody waits for the answer.
            let _ = page.update(cx, |page, cx| answer(page, outcome, cx));
        })
        .detach();
    }
}

/// A plugin host under `dir` with the installed plugins loaded and secrets kept in memory, as
/// the app's own would be without its process-wide wiring: for tests of the plugin pages.
#[cfg(test)]
pub(crate) fn test_plugin_host(dir: &Path) -> (Arc<PluginHost>, Arc<test_support::MemorySecrets>) {
    let sink = |_: Option<&str>, _: PluginEvent| {};
    let host = PluginHost::new(dir.join("plugins"), Arc::new(sink), ide_plugin::Limits::default())
        .expect("the plugin host starts");
    let secrets = Arc::new(test_support::MemorySecrets::default());
    host.set_secret_store(secrets.clone());
    host.load_all();
    host.startup_finished();
    (Arc::new(host), secrets)
}

/// What the tests of plugin features share: copies of the fixture plugins in a temporary home, the
/// app's plugin host started there, linking a copy, and a trusted project in an active window.
#[cfg(test)]
pub(crate) mod test_support {
    use super::super::Workbench;
    use crate::backend::Backend;
    use crate::platform::actions::ActionRegistry;
    use crate::plugins::{Plugins, plugin_action_id};
    use gpui_kit::{App, Entity, TestAppContext, VisualTestContext};
    use ide_host::plugins::{Consent, SecretStore};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Secrets kept in memory, by (plugin, name).
    #[derive(Default)]
    pub struct MemorySecrets(Mutex<HashMap<(String, String), String>>);

    impl MemorySecrets {
        pub fn value(&self, plugin: &str, name: &str) -> Option<String> {
            self.0.lock().get(&(plugin.to_owned(), name.to_owned())).cloned()
        }
    }

    impl SecretStore for MemorySecrets {
        fn get(&self, plugin: &str, name: &str) -> Option<String> {
            self.value(plugin, name)
        }

        fn set(&self, plugin: &str, name: &str, value: Option<&str>) -> Result<(), String> {
            let key = (plugin.to_owned(), name.to_owned());
            match value {
                Some(value) => self.0.lock().insert(key, value.to_owned()),
                None => self.0.lock().remove(&key),
            };
            Ok(())
        }
    }

    /// What a tracing subscriber writes, kept to be read back.
    #[derive(Clone, Default)]
    pub struct CapturedLog(std::sync::Arc<Mutex<Vec<u8>>>);

    impl CapturedLog {
        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock()).into_owned()
        }
    }

    impl std::io::Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The repository, for the fixture plugins checked into it (only ever read).
    pub fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    /// Copies the folder `from` into `to`, recursively, without build output (`target`) or `.git`.
    pub fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("the folder is made");
        for entry in std::fs::read_dir(from).expect("the fixture is readable").flatten() {
            let (source, target) = (entry.path(), to.join(entry.file_name()));
            if source.is_dir() {
                if entry.file_name() == "target" || entry.file_name() == ".git" {
                    continue;
                }
                copy_dir(&source, &target);
            } else {
                std::fs::copy(&source, &target).expect("the file is copied");
            }
        }
    }

    /// A copy of the fixture plugin `name` (under `crates/ide-plugin/tests/fixtures`, else
    /// `plugins/official`) at `<home>/fixtures/<name>`, its manifest rewritten by `edit`.
    pub fn plugin_copy(home: &Path, name: &str, edit: impl Fn(&str) -> String) -> PathBuf {
        let fixture = repo().join("crates/ide-plugin/tests/fixtures").join(name);
        let source = if fixture.is_dir() { fixture } else { repo().join("plugins/official").join(name) };
        let target = home.join("fixtures").join(name);
        copy_dir(&source, &target);
        let manifest = target.join("plugin.toml");
        let text = std::fs::read_to_string(&manifest).expect("the manifest is readable");
        std::fs::write(&manifest, edit(&text)).expect("the manifest is written");
        target
    }

    /// A test's temporary home whose app plugin host is the only one in the test binary while the
    /// home is kept (see [`crate::plugins::lock_test_hosts`]).
    pub struct PluginHome {
        home: tempfile::TempDir,
        _hosts: parking_lot::MutexGuard<'static, ()>,
    }

    impl PluginHome {
        /// `home`, for a test that holds `hosts` from before its plugin host starts.
        pub fn new(home: tempfile::TempDir, hosts: parking_lot::MutexGuard<'static, ()>) -> Self {
            Self { home, _hosts: hosts }
        }
    }

    impl std::ops::Deref for PluginHome {
        type Target = tempfile::TempDir;

        fn deref(&self) -> &tempfile::TempDir {
            &self.home
        }
    }

    /// The workbench tests' temporary home with the app's plugin host started in it, once its first
    /// listing is in. The host keeps plugin secrets in files under the home.
    pub fn start_with_plugins(cx: &mut TestAppContext) -> PluginHome {
        let hosts = crate::plugins::lock_test_hosts();
        let home = crate::workbench::tests::start(cx);
        cx.update(|cx| {
            crate::theme::appearance::init(cx);
            Plugins::init(cx).expect("the plugin host starts");
        });
        settle_app(cx, "the first listing", |cx| cx.global::<Plugins>().snapshot().is_some());
        PluginHome::new(home, hosts)
    }

    /// Waits up to ten seconds for `done`, letting the app's tasks run.
    pub fn settle_app(cx: &mut TestAppContext, what: &str, done: impl Fn(&mut App) -> bool) {
        for _ in 0..1000 {
            cx.run_until_parked();
            if cx.update(|cx| done(cx)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{what}: not within ten seconds");
    }

    /// Links `folder` on the app's plugin host and agrees to it with its required capabilities and
    /// `optional`; waits until its command `command` is an action. Returns the plugin's id.
    pub fn link(cx: &mut TestAppContext, folder: &Path, optional: &[&str], command: &str) -> String {
        let id = cx.update(|cx| {
            let host = cx.global::<Plugins>().host.clone();
            let info = host.link(folder).expect("the plugin links");
            let consent =
                Consent { optional: optional.iter().map(|c| (*c).to_owned()).collect(), signer_change: false };
            let info = host.grant(&info.id, &consent).expect("consent is recorded");
            assert_eq!(info.error, None, "the plugin loads");
            info.id
        });
        let action = plugin_action_id(&id, command);
        settle_app(cx, "the plugin's commands are actions", |cx| cx.global::<ActionRegistry>().contains(&action));
        id
    }

    /// An executable `name` in `bin` running `script`.
    #[cfg(unix)]
    pub fn program(bin: &Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(bin).expect("the folder is made");
        let path = bin.join(name);
        std::fs::write(&path, script).expect("the script is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("the script runs");
    }

    /// Programs a plugin may be granted are looked up in `bin` only.
    pub fn programs_in(bin: &Path, cx: &mut TestAppContext) {
        let bin = bin.to_path_buf();
        let path = move || Some(bin.clone().into_os_string());
        cx.update(|cx| cx.global::<Plugins>().host.set_program_path(std::sync::Arc::new(path)));
    }

    /// A trusted project of `files` (path, text), open in an active window known as `label`.
    pub fn trusted_project<'a>(
        cx: &'a mut TestAppContext,
        label: &str,
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
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new(label, window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        crate::workbench::tests::wait_long(cx, "the project opens", |cx| {
            workbench.read_with(cx, |w, _| w.project_tree().is_some())
        });
        (folder, root, workbench, cx)
    }
}

#[cfg(test)]
mod tests {
    mod project_plugins_per_window;
    mod proofs6_plugins;
    mod proofs6_plugins_market;
    mod proofs9_settings_plugins;

    use super::test_support::{CapturedLog, link, plugin_copy, start_with_plugins, trusted_project};
    use super::*;
    use crate::plugin_models::consent::Risk;
    use crate::plugin_models::plugins_page::{ListRow, Section};
    use crate::workbench::tests::{click_on, notification_titles, run_action, start, wait_long};
    use crate::workbench::welcome_pages::{CustomizeSelect, WelcomePage};
    use gpui_kit::{ClipboardItem, TestAppContext, VisualTestContext};
    use ide_host::plugins::PluginStatus;
    use std::cell::RefCell;
    use std::rc::Rc;

    const AI: &str = "openidea.ai-completion";
    const STRING_TOOLS: &str = "openidea.string-tools";
    const CHECKSTYLE: &str = "openidea.checkstyle";
    /// Typed as keystrokes: letters and digits only (a `-` would read as a modifier separator).
    const SECRET: &str = "sk5f3a9ctest";

    /// A copy of fixture plugin `name` in the temporary home `home`.
    fn fixture(home: &tempfile::TempDir, name: &str) -> PathBuf {
        plugin_copy(home.path(), name, str::to_owned)
    }

    /// A window showing the Welcome screen whose Plugins page works on `host`, shown.
    fn plugins_window<'a>(
        cx: &'a mut TestAppContext,
        host: &Arc<PluginHost>,
    ) -> (Entity<Workbench>, Entity<PluginsPage>, &'a mut VisualTestContext) {
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        let host = Arc::clone(host);
        workbench.update_in(cx, |workbench, window, cx| workbench.use_plugin_host(host, window, cx));
        let page = workbench.read_with(cx, |w, _| w.welcome_plugins_page().cloned()).expect("the Plugins page");
        click_on(cx, "welcome-nav-plugins");
        (workbench, page, cx)
    }

    fn listed(page: &Entity<PluginsPage>, id: &str, cx: &mut VisualTestContext) -> bool {
        page.update(cx, |page, cx| page.details(cx)).is_some_and(|details| details.id == id)
    }

    fn secrets_section(page: &Entity<PluginsPage>, cx: &mut VisualTestContext) -> Option<Section> {
        let details = page.update(cx, |page, cx| page.details(cx))?;
        details.sections.into_iter().find(|section| section.title == "Secrets")
    }

    fn installed(host: &PluginHost, id: &str) -> Option<ide_host::plugins::PluginInfo> {
        host.list(None).plugins.into_iter().find(|plugin| plugin.id == id)
    }

    fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    /// The Debug output of every event the page emits from now on.
    fn record_events(page: &Entity<PluginsPage>, cx: &mut VisualTestContext) -> Rc<RefCell<Vec<String>>> {
        let seen: Rc<RefCell<Vec<String>>> = Rc::default();
        let log = Rc::clone(&seen);
        cx.update(|_, cx| {
            cx.subscribe(page, move |_, event: &PluginsPageEvent, _| log.borrow_mut().push(format!("{event:?}")))
                .detach();
        });
        seen
    }

    /// A secret typed into the Secrets section of a plugin is stored through the plugin host, and
    /// the page then says it is stored. What the field paints is bullets, Copy and Cut leave the
    /// clipboard alone, and the value is in no event's debug output, in no log line and in nothing
    /// the page lists.
    #[gpui_kit::test]
    async fn a_secret_typed_into_the_page_is_stored_through_the_host_and_never_shown_emitted_or_logged(
        cx: &mut TestAppContext,
    ) {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        let home = start(cx);
        let dir = tempfile::tempdir().expect("a folder for the plugins");
        let (host, secrets) = test_plugin_host(dir.path());
        host.install_from_dir(&fixture(&home, "ai-completion"), &Consent::all()).expect("the plugin installs");
        let (_workbench, page, cx) = plugins_window(cx, &host);
        let events = record_events(&page, cx);
        wait_long(cx, "the plugin and its secret are listed", |cx| {
            listed(&page, AI, cx) && secrets_section(&page, cx).is_some_and(|section| !section.secrets.is_empty())
        });
        let row = secrets_section(&page, cx).expect("the Secrets section").secrets[0].clone();
        assert_eq!((row.name.as_str(), row.set), ("api-key", false));

        click_on(cx, "plugins-secret-field-api-key");
        cx.simulate_input(SECRET);
        let field = page.read_with(cx, |page, _| page.secret_input(AI, "api-key", false).cloned()).expect("the field");
        assert_eq!(field.read_with(cx, |field, cx| field.text(cx).to_owned()), SECRET);
        assert_eq!(field.read_with(cx, |field, cx| field.shown(cx)), "\u{2022}".repeat(SECRET.len()));
        cx.write_to_clipboard(ClipboardItem::new_string("before".to_owned()));
        cx.simulate_keystrokes("cmd-a");
        let painted = field.read_with(cx, |field, cx| field.painted(cx));
        assert!(painted.iter().all(|part| part.chars().all(|c| c == '\u{2022}')), "{painted:?}");
        cx.simulate_keystrokes("cmd-c");
        cx.simulate_keystrokes("cmd-x");
        assert_eq!(clipboard_text(cx).as_deref(), Some("before"), "Copy and Cut leave the clipboard alone");
        assert_eq!(field.read_with(cx, |field, cx| field.text(cx).to_owned()), SECRET, "Cut removes nothing");
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the secret is stored", |_| secrets.value(AI, "api-key").as_deref() == Some(SECRET));
        wait_long(cx, "the page says it is stored", |cx| {
            secrets_section(&page, cx).is_some_and(|section| section.secrets.first().is_some_and(|row| row.set))
        });
        assert_eq!(host.secrets(AI).expect("the secrets' state"), [("api-key".to_owned(), true)]);
        assert_eq!(field.read_with(cx, |field, cx| field.text(cx).to_owned()), "", "the field is emptied");

        let emitted = events.borrow().join("\n");
        assert!(emitted.contains("SetSecret"), "{emitted}");
        assert!(!emitted.contains(SECRET), "an event's debug output names the secret: {emitted}");
        let details = page.update(cx, |page, cx| format!("{:?}", page.details(cx)));
        assert!(!details.contains(SECRET));
        let logged = log.text();
        assert!(!logged.contains(SECRET), "the log names the secret: {logged}");
    }

    /// The app's plugin host keeps a plugin's secrets in a file under the temporary home (never in
    /// the login keychain): stored through the host, the secret is in that file and the host says
    /// it is set.
    #[gpui_kit::test]
    async fn the_app_plugin_host_keeps_secrets_in_a_file_under_the_temporary_home(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let folder = fixture(&home, "ai-completion");
        let id = link(cx, &folder, &[], "toggle");
        let host = cx.update(|cx| cx.global::<Plugins>().host.clone());
        host.set_secret(&id, "api-key", Some(SECRET)).expect("the secret is stored");
        assert_eq!(host.secrets(&id).expect("the secrets' state"), [("api-key".to_owned(), true)]);
        let file = home.path().join("data/plugins/.data").join(&id).join("secrets.json");
        let stored = std::fs::read_to_string(&file).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        assert!(stored.contains("api-key"), "{stored}");
        host.set_secret(&id, "api-key", None).expect("the secret is removed");
        assert_eq!(host.secrets(&id).expect("the secrets' state"), [("api-key".to_owned(), false)]);
    }

    /// Uninstall asks first; confirmed, the plugin is removed from the host and from the list.
    #[gpui_kit::test]
    async fn uninstall_asks_and_removes_the_plugin(cx: &mut TestAppContext) {
        let home = start(cx);
        let dir = tempfile::tempdir().expect("a folder for the plugins");
        let (host, _) = test_plugin_host(dir.path());
        host.install_from_dir(&fixture(&home, "string-tools"), &Consent::all()).expect("the plugin installs");
        let (_workbench, page, cx) = plugins_window(cx, &host);
        wait_long(cx, "the plugin is listed", |cx| listed(&page, STRING_TOOLS, cx));
        click_on(cx, "plugins-details-uninstall");
        assert!(cx.debug_bounds("plugins-details-confirm-uninstall").is_some(), "it asks first");
        assert!(installed(&host, STRING_TOOLS).is_some(), "nothing is removed before the answer");
        click_on(cx, "plugins-details-confirm-uninstall");
        wait_long(cx, "the plugin is gone", |cx| {
            installed(&host, STRING_TOOLS).is_none() && !listed(&page, STRING_TOOLS, cx)
        });
    }

    /// A project's own plugins (under `.openidea/plugins`) load only while the project is trusted:
    /// in Restricted Mode the Plugins page lists none of them and the host loads none; Trust Project
    /// loads them; Revoke Trust unloads them again.
    #[gpui_kit::test]
    async fn a_projects_own_plugins_load_only_while_the_project_is_trusted(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        super::test_support::copy_dir(&fixture(&home, "hello"), &root.join(".openidea/plugins/hello"));
        cx.update(|cx| cx.global::<Backend>().host.trust.decide(&root, false, false)).expect("previewed");
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(root.clone(), window, cx));
        wait_long(cx, "the project opens", |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        assert!(workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        let loaded = |cx: &mut VisualTestContext| {
            let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
            host.list(Some(&root)).plugins.iter().any(|plugin| plugin.id == "hello")
        };
        run_action(cx, "plugins.settings", None);
        let page = workbench.read_with(cx, |w, _| w.settings_plugins_page().cloned()).expect("the Plugins page");
        let lists_hello = |cx: &mut VisualTestContext| {
            page.update(cx, |page, cx| page.rows(cx))
                .into_iter()
                .any(|row| matches!(row, ListRow::Item(item) if item.id == "hello"))
        };
        wait_long(cx, "the listing", |cx| !page.update(cx, |page, cx| page.rows(cx)).is_empty());
        assert!(!lists_hello(cx) && !loaded(cx), "a restricted project's plugin is not loaded");

        run_action(cx, "project.trust", None);
        wait_long(cx, "the trust", |cx| workbench.read_with(cx, |w, cx| !w.project_restricted(cx)));
        run_action(cx, "plugins.reloadAll", None);
        wait_long(cx, "the project's plugin is listed", |cx| lists_hello(cx) && loaded(cx));

        run_action(cx, "project.revokeTrust", None);
        wait_long(cx, "the plugin is unloaded", |cx| !loaded(cx));
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// The checkbox of a plugin's row disables it on the host, and enables it again.
    #[gpui_kit::test]
    async fn the_rows_checkbox_disables_and_enables_the_plugin(cx: &mut TestAppContext) {
        let home = start(cx);
        let dir = tempfile::tempdir().expect("a folder for the plugins");
        let (host, _) = test_plugin_host(dir.path());
        host.install_from_dir(&fixture(&home, "string-tools"), &Consent::all()).expect("the plugin installs");
        let (_workbench, page, cx) = plugins_window(cx, &host);
        wait_long(cx, "the plugin is listed", |cx| listed(&page, STRING_TOOLS, cx));
        click_on(cx, "plugins-check-openidea.string-tools");
        wait_long(cx, "the plugin is disabled", |_| installed(&host, STRING_TOOLS).is_some_and(|p| !p.enabled));
        wait_long(cx, "the page is done with it", |cx| {
            page.update(cx, |page, cx| page.details(cx)).is_some_and(|details| !details.busy)
        });
        click_on(cx, "plugins-check-openidea.string-tools");
        wait_long(cx, "the plugin is enabled", |_| {
            installed(&host, STRING_TOOLS).is_some_and(|p| p.enabled && p.status != PluginStatus::Disabled)
        });
    }

    /// Reload Plugins reloads every listed plugin on the window's host. A plugin whose manifest no
    /// longer parses still reloads: the host lists it as failed, with the reason, and the page shows
    /// it so; nothing is reported as a reload failure.
    #[gpui_kit::test]
    async fn reload_plugins_lists_a_plugin_whose_manifest_no_longer_parses_as_failed(cx: &mut TestAppContext) {
        let home = start(cx);
        let dir = tempfile::tempdir().expect("a folder for the plugins");
        let (host, _) = test_plugin_host(dir.path());
        let folder = fixture(&home, "hello");
        host.link(&folder).expect("the plugin links");
        let (workbench, page, cx) = plugins_window(cx, &host);
        wait_long(cx, "the plugin is listed", |cx| listed(&page, "hello", cx));
        std::fs::write(folder.join("plugin.toml"), "id = \n").expect("the manifest is broken");
        run_action(cx, "plugins.reloadAll", None);
        let failed_row = |cx: &mut VisualTestContext| {
            page.update(cx, |page, cx| page.rows(cx)).into_iter().any(|row| match row {
                ListRow::Item(item) => item.tags.iter().any(|tag| tag.label == "Failed"),
                _ => false,
            })
        };
        wait_long(cx, "the page lists the plugin as failed", failed_row);
        let reloaded = host.list(None).plugins;
        assert!(installed(&host, "hello").is_none(), "the old manifest's plugin is gone: {reloaded:?}");
        let broken = reloaded.iter().find(|p| p.status == PluginStatus::Failed).expect("the broken plugin is listed");
        assert!(broken.error.is_some(), "the reason is kept: {broken:?}");
        let titles = notification_titles(&workbench, cx);
        assert!(!titles.iter().any(|title| title.starts_with("Could not reload")), "{titles:?}");
    }

    /// Install Plugin from Disk… shows the install page with the high-risk permission first.
    /// Cancelled, nothing is installed; accepted, the plugin is installed and listed.
    #[gpui_kit::test]
    async fn installing_from_disk_asks_for_consent_with_high_risk_first_and_installs_only_when_accepted(
        cx: &mut TestAppContext,
    ) {
        let home = start(cx);
        let dir = tempfile::tempdir().expect("a folder for the plugins");
        let (host, _) = test_plugin_host(dir.path());
        // `process:checkstyle` is resolved in an empty folder only, never on the user's PATH.
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).expect("the folder is made");
        host.set_program_path(Arc::new(move || Some(bin.clone().into_os_string())));
        let package = path_text(&fixture(&home, "checkstyle"));
        let (workbench, page, cx) = plugins_window(cx, &host);
        let consent = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.plugins_consent_dialog().cloned());

        run_action(cx, "plugins.installFromDisk", Some(json!(package)));
        wait_long(cx, "the install page shows", |cx| consent(cx).is_some());
        let dialog = consent(cx).expect("the install page");
        let shown: Vec<(String, Risk)> =
            dialog.read_with(cx, |d, _| d.capabilities().iter().map(|c| (c.id.clone(), c.risk)).collect());
        assert_eq!(shown.first().map(|(id, risk)| (id.as_str(), *risk)), Some(("process:checkstyle", Risk::High)));
        assert!(shown.windows(2).all(|pair| pair[0].1 >= pair[1].1), "from the highest risk down: {shown:?}");
        click_on(cx, "consent-button-cancel");
        assert!(consent(cx).is_none(), "Cancel closes the install page");
        wait_long(cx, "the install is over", |cx| page.read_with(cx, |page, _| !page.installing()));
        assert!(installed(&host, CHECKSTYLE).is_none(), "declined, nothing is installed");

        run_action(cx, "plugins.installFromDisk", Some(json!(package)));
        wait_long(cx, "the install page shows again", |cx| consent(cx).is_some());
        click_on(cx, "consent-button-ok");
        wait_long(cx, "the plugin is installed", |_| installed(&host, CHECKSTYLE).is_some());
        wait_long(cx, "and listed", |cx| listed(&page, CHECKSTYLE, cx));
    }

    /// Install Plugin from Disk… given a plugin folder's `plugin.toml` (src/features/plugins/store.ts,
    /// `installFromDisk` and `packageFolder`) installs that folder on the app's plugin host from a
    /// project window: the install page asks, and once accepted the plugin is listed on the page the
    /// action drives and its commands are actions.
    #[gpui_kit::test]
    async fn install_from_disk_takes_a_plugin_folder_by_its_manifest_and_makes_its_commands_actions(
        cx: &mut TestAppContext,
    ) {
        let home = start_with_plugins(cx);
        let manifest = path_text(&fixture(&home, "hello").join("plugin.toml"));
        let (_folder, _root, workbench, cx) = trusted_project(cx, "w1", &[("a.txt", "a\n")]);
        let consent = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.plugins_consent_dialog().cloned());
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        run_action(cx, "plugins.installFromDisk", Some(json!(manifest)));
        wait_long(cx, "the install page shows", |cx| consent(cx).is_some());
        click_on(cx, "consent-button-ok");
        wait_long(cx, "the plugin is installed", |_| installed(&host, "hello").is_some());
        let page = workbench
            .read_with(cx, |w, _| w.plugin_ui.background.as_ref().map(|hosted| hosted.page.clone()))
            .expect("the page the action drives");
        wait_long(cx, "the plugin is listed", |cx| listed(&page, "hello", cx));
        let action = plugin_action_id("hello", "insert-date");
        wait_long(cx, "its commands are actions", |cx| {
            cx.update(|_, cx| cx.global::<ActionRegistry>().contains(&action))
        });
    }

    /// The Welcome screen lists Projects and Customize, and Plugins only once the window has a plugin
    /// host; Plugins then shows the Plugins page in place of the projects, and Projects brings them back.
    #[gpui_kit::test]
    async fn the_welcome_screen_lists_projects_and_customize_and_plugins_only_with_a_plugin_host(
        cx: &mut TestAppContext,
    ) {
        let _home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        let welcome = workbench.read_with(cx, |w, _| w.welcome().cloned()).expect("the Welcome screen shows");
        let pages = |cx: &mut VisualTestContext| welcome.read_with(cx, |view, _| view.nav().pages());
        assert_eq!(pages(cx), [WelcomePage::Projects, WelcomePage::Customize]);
        assert!(cx.debug_bounds("welcome-nav-projects").is_some());
        assert!(cx.debug_bounds("welcome-nav-customize").is_some());
        assert!(cx.debug_bounds("welcome-nav-plugins").is_none(), "no plugin host, no Plugins");

        let plugins = tempfile::tempdir().expect("a folder for the plugins");
        let (host, _) = test_plugin_host(plugins.path());
        workbench.update_in(cx, |w, window, cx| w.use_plugin_host(host, window, cx));
        assert_eq!(pages(cx), [WelcomePage::Projects, WelcomePage::Customize, WelcomePage::Plugins]);
        click_on(cx, "welcome-nav-plugins");
        assert_eq!(welcome.read_with(cx, |view, _| view.nav().shown()), WelcomePage::Plugins);
        assert!(cx.debug_bounds("plugins-tab-installed").is_some(), "the Plugins page is embedded");
        assert!(cx.debug_bounds("welcome-brand").is_some(), "beside the sidebar");
        click_on(cx, "welcome-nav-projects");
        assert_eq!(welcome.read_with(cx, |view, _| view.nav().shown()), WelcomePage::Projects);
        assert!(cx.debug_bounds("plugins-tab-installed").is_none());
    }

    /// Customize › Theme and IDE font write the global settings: the theme chosen is the one the
    /// settings name and the windows are drawn with, and the font size is stored with "Use custom
    /// font" on. The page then shows what is stored.
    #[gpui_kit::test]
    async fn customize_changes_the_theme_and_the_font_size_through_the_global_settings(cx: &mut TestAppContext) {
        let home = start(cx);
        cx.update(crate::theme::appearance::init);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        click_on(cx, "welcome-nav-customize");
        let welcome = workbench.read_with(cx, |w, _| w.welcome().cloned()).expect("the Welcome screen shows");
        let customize = welcome.read_with(cx, |view, _| view.customize().clone());
        let dark = |cx: &mut VisualTestContext| cx.update(|_, cx| cx.global::<crate::theme::ActiveTheme>().is_dark());
        assert!(!dark(cx));

        click_on(cx, "customize-theme");
        let choices = customize.update(cx, |page, cx| page.choices(CustomizeSelect::Theme, cx));
        let index =
            choices.iter().position(|choice| !choice.heading && choice.value == "dark").expect("Dark is offered");
        let option: &'static str = Box::leak(format!("customize-option-{index}").into_boxed_str());
        click_on(cx, option);
        assert_eq!(cx.update(|_, cx| crate::platform::settings::global(cx).appearance.theme), "dark");
        wait_long(cx, "the windows are drawn dark", dark);

        click_on(cx, "customize-font-size");
        // The sizes start at 10: the sixth is 15.
        click_on(cx, "customize-option-5");
        let appearance = cx.update(|_, cx| crate::platform::settings::global(cx).appearance);
        assert_eq!((appearance.ui_font_size, appearance.use_custom_font), (15.0, true));
        let stored = std::fs::read_to_string(home.path().join("config/settings.toml")).unwrap_or_default();
        assert!(stored.contains("uiFontSize") && stored.contains("useCustomFont = true"), "{stored}");
        let shown = customize.read_with(cx, |page, _| page.view());
        assert_eq!((shown.theme.as_str(), shown.font_size), ("dark", 15.0), "the page shows what is stored");
    }

    /// Customize › All settings… opens the Settings dialog over the Welcome screen.
    #[gpui_kit::test]
    async fn all_settings_on_customize_opens_the_settings_dialog(cx: &mut TestAppContext) {
        let _home = start(cx);
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        click_on(cx, "welcome-nav-customize");
        let open = |cx: &mut VisualTestContext| workbench.read_with(cx, |w, _| w.settings_dialog().is_some());
        assert!(!open(cx));
        click_on(cx, "customize-all-settings");
        wait_long(cx, "All settings… opens Settings", open);
        assert!(workbench.read_with(cx, |w, _| w.welcome().is_some()), "over the Welcome screen");
    }

    /// The own plugins a project carries for these tests: Hello (a command, a Tools menu entry, a
    /// status item and a view) and Model Profiles (a settings page), under `.openidea/plugins`.
    fn project_with_own_plugins(home: &tempfile::TempDir) -> (tempfile::TempDir, PathBuf) {
        let folder = tempfile::tempdir().expect("a temporary folder");
        let root = folder.path().canonicalize().expect("the folder exists");
        let tools = |text: &str| format!("{text}\n[[contributes.menus]]\nmenu = \"tools\"\ncommand = \"word-count\"\n");
        let hello = plugin_copy(home.path(), "hello", tools);
        super::test_support::copy_dir(&hello, &root.join(".openidea/plugins/hello"));
        let profiles = fixture(home, "settings-pages");
        super::test_support::copy_dir(&profiles, &root.join(".openidea/plugins/settings-pages"));
        (folder, root)
    }

    /// What of the project's own plugins the window has: the plugins loaded on the host, their
    /// command as an action, the Tools menu entry, the status item, the view's tool window and the
    /// settings page.
    #[derive(Debug, PartialEq, Eq)]
    struct OwnPlugins {
        loaded: bool,
        action: bool,
        menu: bool,
        status: bool,
        tool_window: bool,
        settings_page: bool,
    }

    const WORD_COUNT: &str = "plugin.hello.word-count";
    const ALL: OwnPlugins =
        OwnPlugins { loaded: true, action: true, menu: true, status: true, tool_window: true, settings_page: true };
    const NONE: OwnPlugins = OwnPlugins {
        loaded: false,
        action: false,
        menu: false,
        status: false,
        tool_window: false,
        settings_page: false,
    };

    fn own_plugins(workbench: &Entity<Workbench>, root: &Path, cx: &mut VisualTestContext) -> OwnPlugins {
        use super::super::menu;
        use super::super::menu_spec::DYNAMIC_PLUGIN_TOOLS;
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        let listed = host.list(Some(root)).plugins;
        let loaded = ["hello", "settings-pages"].iter().all(|id| listed.iter().any(|plugin| plugin.id == *id));
        let state = cx.update(|window, cx| menu::state(window, cx));
        let menu =
            state.dynamic.get(DYNAMIC_PLUGIN_TOOLS).is_some_and(|items| items.iter().any(|i| i.action == WORD_COUNT));
        let status =
            workbench.read_with(cx, |w, cx| w.status_bar().read(cx).model().spec("plugin.hello/hello").is_some());
        let tool_window = workbench.read_with(cx, |w, _| {
            w.tool_windows().and_then(|layout| layout.window("plugin.hello.greetings")).is_some()
        });
        let settings_page = cx.update(|window, cx| {
            super::super::plugin_contributions::plugin_settings_pages(window, cx)
                .iter()
                .any(|page| page.id == "plugins.settings-pages")
        });
        OwnPlugins {
            loaded,
            action: cx.update(|_, cx| cx.global::<ActionRegistry>().contains(WORD_COUNT)),
            menu,
            status,
            tool_window,
            settings_page,
        }
    }

    /// Agrees to the project's own plugins once they are loaded (each needs the user's consent).
    fn agree_to_own_plugins(root: &Path, cx: &mut VisualTestContext) {
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        wait_long(cx, "the project's plugins are loaded", |_| {
            let listed = host.list(Some(root)).plugins;
            ["hello", "settings-pages"].iter().all(|id| listed.iter().any(|plugin| plugin.id == *id))
        });
        for plugin in host.list(Some(root)).plugins.into_iter().filter(|p| p.status == PluginStatus::NeedsConsent) {
            host.grant(&plugin.id, &Consent::default()).expect("consent is recorded");
        }
    }

    /// A window that opens the project at `root`, trusted beforehand or not decided (`None`).
    fn open_with_own_plugins<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
        trusted: Option<bool>,
    ) -> (Entity<Workbench>, &'a mut VisualTestContext) {
        if let Some(trusted) = trusted {
            cx.update(|cx| cx.global::<Backend>().host.trust.decide(root, trusted, false)).expect("decided");
        }
        let (workbench, cx) = cx.add_window_view(|window, cx| Workbench::new("w1", window, cx));
        cx.update(|window, _| window.activate_window());
        let path = root.to_path_buf();
        workbench.update_in(cx, |workbench, window, cx| workbench.open_project(path, window, cx));
        wait_long(cx, "the project opens", |cx| workbench.read_with(cx, |w, _| w.project_tree().is_some()));
        (workbench, cx)
    }

    /// Lets the plugin host deliver what it queued (it flushes every 250 ms) for about a second.
    fn let_events_flow(cx: &mut VisualTestContext) {
        let until = std::time::Instant::now() + Duration::from_secs(1);
        wait_long(cx, "two seconds", |_| std::time::Instant::now() >= until);
    }

    fn listings(cx: &mut VisualTestContext) -> u64 {
        cx.update(|_, cx| cx.global::<Plugins>().listings())
    }

    /// A trusted project's own plugins load when it opens, without the Plugins page: the command
    /// is an action and runs through the registry, and the menu entry, the status item, the view's
    /// tool window and the settings page are there.
    #[gpui_kit::test]
    async fn a_trusted_projects_own_plugins_load_when_it_opens_and_their_command_runs(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let (_folder, root) = project_with_own_plugins(&home);
        let (workbench, cx) = open_with_own_plugins(cx, &root, Some(true));
        agree_to_own_plugins(&root, cx);
        wait_long(cx, "the project's plugins contribute", |cx| own_plugins(&workbench, &root, cx) == ALL);
        assert!(workbench.read_with(cx, |w, _| w.settings_plugins_page().is_none()), "no Plugins page was opened");
        run_action(cx, WORD_COUNT, None);
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        wait_long(cx, "the command loaded the plugin's code", |_| {
            host.info_of("hello").is_ok_and(|info| info.status == PluginStatus::Active)
        });
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// A project whose trust is not decided opens in Restricted Mode: none of its own plugins loads
    /// or contributes, and the plugins are not listed over and over meanwhile.
    #[gpui_kit::test]
    async fn an_undecided_projects_own_plugins_do_not_load_and_the_listing_does_not_repeat(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let (_folder, root) = project_with_own_plugins(&home);
        let (workbench, cx) = open_with_own_plugins(cx, &root, None);
        assert!(workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        let_events_flow(cx);
        let before = listings(cx);
        let_events_flow(cx);
        let repeated = listings(cx) - before;
        assert!(repeated <= 1, "the plugins were listed {repeated} more times in a quiet second");
        assert_eq!(own_plugins(&workbench, &root, cx), NONE);
        // With the Plugins page open, which lists the window's plugins on every announced change.
        run_action(cx, "plugins.settings", None);
        let page = workbench.read_with(cx, |w, _| w.settings_plugins_page().cloned()).expect("the Plugins page");
        wait_long(cx, "the page's listing", |cx| !page.update(cx, |page, cx| page.rows(cx)).is_empty());
        let before = listings(cx);
        let_events_flow(cx);
        let repeated = listings(cx) - before;
        assert!(repeated <= 1, "with the Plugins page open the plugins were listed {repeated} more times");
        assert_eq!(own_plugins(&workbench, &root, cx), NONE);
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// Trust Project, granted after the project opened in Restricted Mode, loads its own plugins.
    #[gpui_kit::test]
    async fn trusting_an_open_project_loads_its_own_plugins(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let (_folder, root) = project_with_own_plugins(&home);
        let (workbench, cx) = open_with_own_plugins(cx, &root, None);
        let_events_flow(cx);
        assert_eq!(own_plugins(&workbench, &root, cx), NONE);
        run_action(cx, "project.trust", None);
        wait_long(cx, "the trust", |cx| workbench.read_with(cx, |w, cx| !w.project_restricted(cx)));
        agree_to_own_plugins(&root, cx);
        wait_long(cx, "the project's plugins contribute", |cx| own_plugins(&workbench, &root, cx) == ALL);
        let before = listings(cx);
        let_events_flow(cx);
        let repeated = listings(cx) - before;
        assert!(repeated <= 1, "the plugins were listed {repeated} more times in a quiet second");
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// Revoke Trust unloads the project's own plugins: their action, menu entry, status item, tool
    /// window and settings page go.
    #[gpui_kit::test]
    async fn revoking_trust_unloads_the_projects_own_plugins_and_their_contributions(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let (_folder, root) = project_with_own_plugins(&home);
        let (workbench, cx) = open_with_own_plugins(cx, &root, Some(true));
        agree_to_own_plugins(&root, cx);
        wait_long(cx, "the project's plugins contribute", |cx| own_plugins(&workbench, &root, cx) == ALL);
        run_action(cx, "project.revokeTrust", None);
        wait_long(cx, "the project's plugins are gone", |cx| own_plugins(&workbench, &root, cx) == NONE);
        let before = listings(cx);
        let_events_flow(cx);
        let repeated = listings(cx) - before;
        assert!(repeated <= 1, "the plugins were listed {repeated} more times in a quiet second");
        cx.update(|_, cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// Closing the project unloads its own plugins, and what they contributed goes.
    #[gpui_kit::test]
    async fn closing_a_project_unloads_its_own_plugins(cx: &mut TestAppContext) {
        let home = start_with_plugins(cx);
        let (_folder, root) = project_with_own_plugins(&home);
        let (workbench, cx) = open_with_own_plugins(cx, &root, Some(true));
        agree_to_own_plugins(&root, cx);
        wait_long(cx, "the project's plugins contribute", |cx| own_plugins(&workbench, &root, cx) == ALL);
        workbench.update_in(cx, |w, window, cx| w.close_project(window, cx));
        wait_long(cx, "the project's plugins are gone", |cx| {
            let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
            let loaded = host.list(Some(&root)).plugins.iter().any(|plugin| plugin.id == "hello");
            !loaded && !cx.update(|_, cx| cx.global::<ActionRegistry>().contains(WORD_COUNT))
        });
        assert!(
            workbench
                .read_with(cx, |w, _| w.tool_windows().is_none_or(|l| l.window("plugin.hello.greetings").is_none()))
        );
        assert!(!cx.update(|window, cx| {
            super::super::plugin_contributions::plugin_settings_pages(window, cx)
                .iter()
                .any(|page| page.id == "plugins.settings-pages")
        }));
    }
}
