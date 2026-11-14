//! Carries out what the run models ask of the backend: the steps of a [`StartRun`] (before launch
//! tasks, the Java launch preparation, the Gradle init scripts, the process start), the
//! [`StopAction`]s of Stop, console input, and the configurations a project lists. Process output and
//! exits come back as backend events and go into the [`RunStore`], which the Run tool window draws.
//!
//! The driver belongs to a project window. It reports what only the window can do (show the Run tool
//! window, raise a notification, show another tool window) as [`RunDriverEvent`]s.

use crate::backend::Backend;
use crate::platform::actions::{ActionRegistry, execute_action};
use crate::platform::i18n::{I18n, Localization, Param};
use crate::platform::local_time::now_ms;
use crate::platform::storage::UiStorage;
use crate::run::before_launch::{BeforeLaunch, LaunchEnv, LaunchStep};
use crate::run::build_task::{BUILD_LAST_STATUS, BuildRequest, build_outcome};
use crate::run::config_types::{RunConfigTypes, TypeEnv};
use crate::run::configs::param;
use crate::run::console::ChunkKind;
use crate::run::gradle_run::{GradleDelegation, gradle_boot_script, gradle_test_script, launch_with_script};
use crate::run::java_launch::{FoundSource, JavaLaunchError, ModuleStep, PrepareStep, PreparedLaunch};
use crate::run::jvm_types::{gradle_jvm_args_script, with_jvm_args_script};
use crate::run::links::FileTarget;
use crate::run::navigate::{OpenAction, ResolveStep, TargetResolver, open_resolved};
use crate::run::sessions::selected_target;
use crate::run::store::{
    CloseTab, FINISH_DELAY_MS, FinishContext, FlushSchedule, PINNED_KEY, Persist, RECENT_KEY, RunSettings, RunStore,
    SELECTED_KEY, SaveConfigs, SaveStep, SessionStatus, StartEffect, StartEnv, StartOptions, StartRun, StartStep,
    StopAction, StorageWrite, TEMPORARY_KEY,
};
use crate::run::types::register_builtin_types;
use crate::run_testing::parsers::with_test_reporting;
use crate::workbench::notifications::{NotificationLevel, NotificationRequest};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::LocalBoxFuture;
use gpui_kit::{AnyWindowHandle, App, AsyncApp, Context, Entity, EventEmitter, Task, WeakEntity};
use ide_core::settings::Settings;
use ide_core::{IdeError, IdeEvent, RunId};
use ide_host::Host;
use ide_run::{OutputChunk, RunConfig, RunInfo};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// What the driver asks of its window.
#[derive(Clone, Debug, PartialEq)]
pub enum RunDriverEvent {
    /// Show the Run tool window; `focus`: and give it the keyboard.
    ShowRunWindow {
        focus: bool,
    },
    Notify(NotificationRequest),
    /// A before launch task shows this tool window.
    ShowToolWindow(String),
    /// Open a file location printed in the output: 0-based line and UTF-16 column.
    OpenFile {
        path: PathBuf,
        line: u32,
        col: u32,
    },
    /// Run another slice's action: running the current file's context, or the configuration chooser.
    RunAction {
        id: &'static str,
        arg: Option<serde_json::Value>,
    },
}

/// Saves the open documents; the error says why one could not be saved.
pub type SaveAll = Rc<dyn Fn(&mut App) -> LocalBoxFuture<'static, Result<(), String>>>;

/// The Run settings the store reads, from the settings in effect.
pub fn run_settings(settings: &Settings) -> RunSettings {
    RunSettings {
        confirm_rerun: settings.run.confirm_rerun,
        confirm_terminate: settings.run.confirm_terminate,
        show_on_start: settings.run.show_on_start,
        default_target: settings.run.default_target.clone(),
        php_target: settings.languages.php.run_target.clone(),
        node_target: settings.languages.node.run_target.clone(),
    }
}

/// The page a Tomcat configuration with "Open browser after launch" opens once it started.
pub fn tomcat_browser_url(config: &RunConfig) -> String {
    let non_empty = |key: &str| param(config, key).filter(|value| !value.is_empty());
    if let Some(url) = non_empty("browserUrl") {
        return url.to_string();
    }
    let deployed = non_empty("deploymentPath").map(|path| {
        let name = path.replace('\\', "/").rsplit('/').next().unwrap_or_default().to_string();
        match name.len().checked_sub(4).filter(|at| name.get(*at..).is_some_and(|ext| ext.eq_ignore_ascii_case(".war")))
        {
            Some(at) => name[..at].to_string(),
            None => name,
        }
    });
    let name = non_empty("artifactName").map(str::to_string).or(deployed).unwrap_or_default();
    let context = match non_empty("contextPath") {
        Some(context) => context.to_string(),
        None if !name.is_empty() && name != "ROOT" => format!("/{name}"),
        None => "/".to_string(),
    };
    let port = non_empty("httpPort").unwrap_or("8080");
    let path = if context == "/" { "/".to_string() } else { format!("{context}/") };
    format!("http://localhost:{port}{path}")
}

/// The texts of IDEA's "Process 'x' Is Running" question before a rerun: title, message, OK.
fn rerun_question(name: &str, count: usize, i18n: &mut I18n) -> (String, String, String) {
    let title = i18n.tx_built(&["Process '", "' Is Running"], &[Param::from(name)]);
    let message = if count > 1 {
        let count = count.to_string();
        i18n.tx_built(
            &["'", "' is not allowed to run in parallel.\nWould you like to stop ", " running instances?"],
            &[Param::from(name), Param::from(count.as_str())],
        )
    } else {
        i18n.tx_built(
            &["'", "' is not allowed to run in parallel.\nWould you like to stop the running one?"],
            &[Param::from(name)],
        )
    };
    (title, message, i18n.tx("Stop and Rerun"))
}

/// The texts of the question before closing the tab of a running process: title, message, OK.
fn terminate_question(name: &str, i18n: &mut I18n) -> (String, String, String) {
    let title = i18n.tx_built(&["Process '", "' Is Running"], &[Param::from(name)]);
    let message = i18n.tx_built(&["Do you want to terminate the process '", "'?"], &[Param::from(name)]);
    (title, message, i18n.tx("Terminate"))
}

fn flatten<T>(result: anyhow::Result<Result<T, IdeError>>) -> Result<T, IdeError> {
    result.unwrap_or_else(|error| Err(IdeError::internal(error.to_string())))
}

pub struct RunDriver {
    label: String,
    store: Entity<RunStore>,
    types: Rc<RunConfigTypes>,
    /// The window questions are asked in; without one a question is answered "no".
    window: Option<AnyWindowHandle>,
    save_all: Option<SaveAll>,
    /// The tool windows a before launch task may show, as (id, title).
    tool_windows: Vec<(String, String)>,
    /// The Run tool window is showing: a failed exit needs no notification then.
    pub run_window_visible: bool,
    /// Waits for the processes of these sessions to be gone.
    waiters: Vec<(Vec<String>, oneshot::Sender<()>)>,
    /// What feeds each session's output into the store.
    pumps: HashMap<String, Task<()>>,
    /// The pending console flush of each session.
    flushes: HashMap<String, Task<()>>,
    loading: Option<Task<()>>,
    /// What runs once the configurations are listed (or could not be).
    after_load: Vec<AfterLoad>,
}

type AfterLoad = Box<dyn FnOnce(&mut RunDriver, &mut Context<RunDriver>)>;

impl EventEmitter<RunDriverEvent> for RunDriver {}

impl RunDriver {
    pub fn new(label: &str, store: Entity<RunStore>) -> Self {
        let mut types = RunConfigTypes::new();
        register_builtin_types(&mut types);
        Self {
            label: label.to_owned(),
            store,
            types: Rc::new(types),
            window: None,
            save_all: None,
            tool_windows: Vec::new(),
            run_window_visible: false,
            waiters: Vec::new(),
            pumps: HashMap::new(),
            flushes: HashMap::new(),
            loading: None,
            after_load: Vec::new(),
        }
    }

    pub fn with_window(mut self, window: AnyWindowHandle) -> Self {
        self.window = Some(window);
        self
    }

    pub fn with_save_all(mut self, save_all: SaveAll) -> Self {
        self.save_all = Some(save_all);
        self
    }

    pub fn with_tool_windows(mut self, tool_windows: Vec<(String, String)>) -> Self {
        self.tool_windows = tool_windows;
        self
    }

    pub fn store(&self) -> &Entity<RunStore> {
        &self.store
    }

    pub fn types(&self) -> Rc<RunConfigTypes> {
        Rc::clone(&self.types)
    }

    fn settings(&self, cx: &App) -> Settings {
        crate::platform::settings::effective(&self.label, cx)
    }

    fn notify(&mut self, request: NotificationRequest, cx: &mut Context<Self>) {
        cx.emit(RunDriverEvent::Notify(request));
    }

    fn persist(writes: impl IntoIterator<Item = StorageWrite>, cx: &mut App) {
        for write in writes {
            let storage = cx.default_global::<UiStorage>();
            match write.value {
                Some(value) => storage.set(&write.key, value),
                None => storage.remove(&write.key),
            }
        }
    }

    /// Stores what a change to the configurations wrote; temporary configurations are stored without
    /// what settings derived, once the backend stripped it.
    fn carry_out_persist(&mut self, persist: Persist, cx: &mut Context<Self>) {
        Self::persist(persist.writes, cx);
        let Some(strip) = persist.strip_derived else { return };
        let label = self.label.clone();
        cx.spawn(async move |this, cx| {
            let configs = strip.configs.clone();
            let stripped =
                Self::on_host(
                    cx,
                    move |host| async move { ide_host::ops::run::without_derived(&host, &label, configs) },
                );
            let kept = match stripped.await {
                Ok(kept) => kept,
                Err(error) => {
                    tracing::warn!("cannot store the temporary run configurations: {}", error.message);
                    return;
                }
            };
            let _ = this.update(cx, |driver, cx| {
                let write = driver.store.read(cx).derived_stripped(strip.generation, &strip.root, &kept);
                Self::persist(write, cx);
            });
        })
        .detach();
    }

    // ---------------------------------------------------------------------------------------------
    // Configurations

    /// Lists the configurations of the project at `root` (detected and saved ones, then the stored
    /// temporary ones with today's settings), with the stored selection, pinned and recent ones.
    /// A project in Restricted Mode lists none.
    pub fn load(&mut self, root: &str, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.project_root = Some(root.to_owned());
            cx.notify();
        });
        self.apply_console_settings(cx);
        let Some(backend) = cx.try_global::<Backend>() else { return self.load_ended(cx) };
        let trusted = backend.host.project(&self.label).is_ok_and(|project| project.is_trusted());
        if !trusted {
            return self.load_ended(cx);
        }
        let label = self.label.clone();
        let listed = backend.with_host(move |host| async move { ide_host::ops::run::configs(&host, &label) });
        let (label, root) = (self.label.clone(), root.to_owned());
        self.loading = Some(cx.spawn(async move |this, cx| {
            let saved = match flatten(listed.await) {
                Ok(saved) => saved,
                Err(error) => {
                    let _ = this.update(cx, |driver, cx| {
                        driver.store.update(cx, |store, cx| {
                            store.load_failed(&error.message);
                            cx.notify();
                        });
                        driver.load_ended(cx);
                    });
                    return;
                }
            };
            let key = format!("{TEMPORARY_KEY}{root}");
            let stored = cx.update(|cx| cx.try_global::<UiStorage>().and_then(|s| s.get(&key)).map(str::to_owned));
            let kept = RunStore::kept_temporary(&saved, stored.as_deref());
            let temporary =
                if kept.is_empty() {
                    kept
                } else {
                    let applied = kept.clone();
                    let applying = Self::on_host(cx, move |host| async move {
                        ide_host::ops::run::apply_settings(&host, &label, applied)
                    });
                    applying.await.unwrap_or(kept)
                };
            let _ = this.update(cx, |driver, cx| {
                let stored = |prefix: &str| -> Option<String> {
                    cx.try_global::<UiStorage>().and_then(|s| s.get(&format!("{prefix}{root}"))).map(str::to_owned)
                };
                let (selected, pinned, recent) = (stored(SELECTED_KEY), stored(PINNED_KEY), stored(RECENT_KEY));
                driver.store.update(cx, |store, cx| {
                    // Another project opened meanwhile.
                    if store.project_root.as_deref() != Some(root.as_str()) {
                        return;
                    }
                    store.loaded(saved, temporary, selected.as_deref(), pinned.as_deref(), recent.as_deref());
                    cx.notify();
                });
                driver.load_ended(cx);
            });
        }));
    }

    /// Settings › Console › Override console cycle buffer size, applied to every console.
    pub fn apply_console_settings(&mut self, cx: &mut Context<Self>) {
        let console = self.settings(cx).console;
        let limit = console.override_buffer.then(|| console.buffer_kb as usize * 1024);
        self.store.update(cx, |store, cx| {
            if store.console_char_limit != limit {
                store.apply_console_settings(limit);
                cx.notify();
            }
        });
    }

    fn load_ended(&mut self, cx: &mut Context<Self>) {
        self.loading = None;
        for then in std::mem::take(&mut self.after_load) {
            then(self, cx);
        }
    }

    /// Runs `then` once the configurations are listed: now when they are, else after listing them.
    pub fn when_loaded(&mut self, then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static, cx: &mut Context<Self>) {
        if self.store.read(cx).loaded {
            return then(self, cx);
        }
        self.after_load.push(Box::new(then));
        if self.loading.is_none() {
            match self.store.read(cx).project_root.clone() {
                Some(root) => self.load(&root, cx),
                None => self.load_ended(cx),
            }
        }
    }

    /// The project closed: every process stops and the sessions and configurations are forgotten.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        let stops = self.store.update(cx, |store, cx| {
            let stops = store.reset_runs(now_ms());
            store.project_root = None;
            cx.notify();
            stops
        });
        for stop in stops {
            self.carry_out_stop(None, stop, cx);
        }
        self.pumps.clear();
        self.flushes.clear();
        self.loading = None;
        self.after_load.clear();
        self.resolve_waiters(cx);
    }

    // ---------------------------------------------------------------------------------------------
    // Starting

    /// Starts `config`. The task ends with the session it started, if any (which may have failed).
    pub fn start(&mut self, config: RunConfig, options: StartOptions, cx: &mut Context<Self>) -> Task<Option<String>> {
        let settings = run_settings(&self.settings(cx));
        let types = Rc::clone(&self.types);
        let (start, step) = self.store.update(cx, |store, cx| {
            let started = StartRun::start(&config, options, store, StartEnv { types: &types, settings: &settings });
            cx.notify();
            started
        });
        cx.spawn(async move |this, cx| Self::drive(this, start, step, cx).await)
    }

    /// Restarts a session's tab with its configuration as saved now; a derived run keeps its tests.
    pub fn rerun(&mut self, session_id: &str, cx: &mut Context<Self>) -> Option<Task<Option<String>>> {
        let store = self.store.read(cx);
        let session = store.session(session_id)?;
        let config = store.rerun_config(session).clone();
        let base_config = session.base_config.as_ref().map(|_| store.base_config_of(session).clone());
        let options = StartOptions { reuse: Some(session_id.to_owned()), base_config, ..StartOptions::default() };
        Some(self.start(config, options, cx))
    }

    /// Runs backend work that takes the host; an error when there is no backend.
    fn on_host<T, F, W>(cx: &mut AsyncApp, work: W) -> impl Future<Output = Result<T, IdeError>> + use<T, F, W>
    where
        W: FnOnce(Arc<Host>) -> F + Send + 'static,
        T: Send + 'static,
        F: Future<Output = Result<T, IdeError>> + Send + 'static,
    {
        let call = cx.update(|cx| cx.try_global::<Backend>().map(|backend| backend.with_host(work)));
        async move {
            match call {
                Some(call) => flatten(call.await),
                None => Err(IdeError::internal("the backend is not running")),
            }
        }
    }

    /// Calls `act` with the text catalog.
    fn with_i18n<R>(cx: &mut AsyncApp, act: impl FnOnce(&mut I18n) -> R) -> R {
        cx.update(|cx| act(&mut cx.default_global::<Localization>().0))
    }

    /// Run: the selected configuration, restarting its live session; the active file's context when
    /// "Current File" is selected; the configuration chooser when nothing is. The task ends with the
    /// session the launch started, if any.
    pub fn run_selected(&mut self, cx: &mut Context<Self>) -> Task<Option<String>> {
        let store = self.store.read(cx);
        if store.current_file_selected() {
            cx.emit(RunDriverEvent::RunAction { id: "run.runContext", arg: None });
            return Task::ready(None);
        }
        let Some(config) = store.selected_config().cloned() else {
            cx.emit(RunDriverEvent::RunAction { id: "run.chooseConfig", arg: None });
            return Task::ready(None);
        };
        if let Some(live) = selected_target(store, Some(&config.id)).map(|session| session.id.clone()) {
            return self.rerun(&live, cx).unwrap_or_else(|| Task::ready(None));
        }
        self.start(config, StartOptions::default(), cx)
    }

    /// [`Self::run_selected`] once the configurations are listed.
    pub fn launch_selected(&mut self, cx: &mut Context<Self>) -> Task<Option<String>> {
        let (sender, launched) = oneshot::channel();
        self.when_loaded(
            move |driver, cx| {
                // The receiver is gone only when the caller dropped the launch.
                let _ = sender.send(driver.run_selected(cx));
            },
            cx,
        );
        cx.spawn(async move |_, _| match launched.await {
            Ok(launch) => launch.await,
            // The project closed before its configurations were listed.
            Err(_) => None,
        })
    }

    // ---------------------------------------------------------------------------------------------
    // Changing the configurations

    /// Selects a configuration (or "Current File", or nothing) and stores the selection.
    pub fn select_config(&mut self, id: Option<&str>, cx: &mut Context<Self>) {
        let write = self.store.update(cx, |store, cx| {
            let write = store.select_config(id);
            cx.notify();
            write
        });
        Self::persist(write, cx);
    }

    /// A configuration was started under the debugger: it leads the chooser's recent configurations.
    pub fn mark_recent(&mut self, id: &str, cx: &mut Context<Self>) {
        let write = self.store.update(cx, |store, cx| {
            let write = store.mark_recent(id);
            cx.notify();
            write
        });
        Self::persist(write, cx);
    }

    /// Pins a configuration to the top of the chooser, or unpins it.
    pub fn toggle_pinned(&mut self, id: &str, cx: &mut Context<Self>) {
        let write = self.store.update(cx, |store, cx| {
            let write = store.toggle_pinned(id);
            cx.notify();
            write
        });
        Self::persist(write, cx);
    }

    /// Deletes a configuration: a temporary one is forgotten, any other is removed from the project's
    /// saved configurations (a detected one is suppressed), which are listed again.
    pub fn delete_config(&mut self, id: &str, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let (persist, saved) = self.store.update(cx, |store, cx| {
            let deleted = store.delete_config(id);
            cx.notify();
            deleted
        });
        self.carry_out_persist(persist, cx);
        match saved {
            Some((user, removed)) => self.save_configs(user, removed, cx),
            None => Task::ready(Ok(())),
        }
    }

    /// "Save Configuration": the temporary configuration `id` becomes a saved one, and stays selected.
    pub fn save_temporary(&mut self, id: &str, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let saved = self.store.update(cx, |store, cx| {
            let saved = store.save_temporary(id);
            cx.notify();
            saved
        });
        let Some((persist, user)) = saved else { return Task::ready(Ok(())) };
        self.carry_out_persist(persist, cx);
        let saving = self.save_configs(user, Vec::new(), cx);
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            saving.await?;
            this.update(cx, |driver, cx| driver.select_config(Some(&id), cx)).map_err(|error| error.to_string())
        })
    }

    /// Edit Configurations' OK / Apply: saves the dialog's working copy `draft`, suppressing the detected
    /// configurations `removed_detected_ids`, and lists the configurations again.
    pub fn save_edited_configs(
        &mut self,
        draft: &[RunConfig],
        removed_detected_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let (persist, user) = self.store.update(cx, |store, cx| {
            let saved = store.save_edited_configs(draft);
            cx.notify();
            saved
        });
        self.carry_out_persist(persist, cx);
        self.save_configs(user, removed_detected_ids, cx)
    }

    /// The edit dialog closed: a configuration it was opened to modify and that was not saved goes back
    /// to an ordinary temporary one.
    pub fn editor_closed(&mut self, cx: &mut Context<Self>) {
        let persist = self.store.update(cx, |store, cx| {
            let persist = store.editor_closed();
            cx.notify();
            persist
        });
        self.carry_out_persist(persist, cx);
    }

    /// The project's configuration templates: per-type defaults for added configurations.
    pub fn load_templates(&self, cx: &mut Context<Self>) -> Task<Result<Vec<RunConfig>, String>> {
        let label = self.label.clone();
        cx.spawn(async move |_, cx| {
            Self::on_host(cx, move |host| async move { ide_host::ops::run::run_templates(&host, &label) })
                .await
                .map_err(|error| error.message)
        })
    }

    /// Saves `user` as the project's configurations (an application without a module gets the module of
    /// its main class first), suppressing the detected `removed`, then lists them again. A failure is
    /// told in a notification too.
    pub fn save_configs(
        &mut self,
        user: Vec<RunConfig>,
        removed: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        cx.spawn(async move |this, cx| {
            if let Err(error) = Self::write_configs(&this, user, removed, cx).await {
                let mut request = NotificationRequest::new(NotificationLevel::Error, "Cannot save run configurations");
                request.group = Some("run".to_owned());
                request.body = Some(error.clone());
                let _ = this.update(cx, |driver, cx| driver.notify(request, cx));
                return Err(error);
            }
            let listed = this.update(cx, |driver, cx| driver.reload(cx)).map_err(|error| error.to_string())?;
            // The sender goes only with the driver: there is nothing left to list then.
            let _ = listed.await;
            Ok(())
        })
    }

    /// Answers the steps of a [`SaveConfigs`] and writes what it settles on.
    async fn write_configs(
        this: &WeakEntity<Self>,
        user: Vec<RunConfig>,
        removed: Vec<String>,
        cx: &mut AsyncApp,
    ) -> Result<(), String> {
        let label = this.update(cx, |driver, _| driver.label.clone()).map_err(|error| error.to_string())?;
        let (mut save, mut step) = SaveConfigs::start(user, removed);
        loop {
            step = match step {
                SaveStep::Save { configs, removed_detected_ids } => {
                    let saving = Self::on_host(cx, move |host| async move {
                        ide_host::ops::run::save_configs(&host, &label, &configs, &removed_detected_ids)
                    });
                    return saving.await.map_err(|error| error.message);
                }
                // A search that fails finds no source: the configuration is saved without a module.
                SaveStep::Module(ModuleStep::FindFiles { names, limit }) => {
                    let found = Self::find_sources(cx, label.clone(), names, limit).await;
                    save.files_found(found.unwrap_or_else(|error| {
                        tracing::warn!("cannot look for the main class of a run configuration: {}", error.message);
                        Vec::new()
                    }))
                }
                SaveStep::Module(ModuleStep::ListConfigs) => {
                    let configs = this
                        .update(cx, |driver, cx| driver.store.read(cx).configs().to_vec())
                        .map_err(|error| error.to_string())?;
                    save.configs_listed(configs)
                }
                SaveStep::Module(ModuleStep::ReadSources { paths }) => {
                    let texts = Self::read_sources(cx, label.clone(), paths).await.map_err(|error| error.message)?;
                    // The build model's module directories are not loaded in this window.
                    save.sources_read(&texts, &[])
                }
                SaveStep::Module(ModuleStep::Done(_)) => {
                    return Err("the module search of a configuration ended without its answer".to_owned());
                }
            };
        }
    }

    /// Lists the configurations again; resolves once they are listed, or could not be.
    fn reload(&mut self, cx: &mut Context<Self>) -> oneshot::Receiver<()> {
        let (sender, listed) = oneshot::channel();
        let Some(root) = self.store.read(cx).project_root.clone() else {
            let _ = sender.send(());
            return listed;
        };
        self.after_load.push(Box::new(move |_, _| {
            let _ = sender.send(());
        }));
        self.load(&root, cx);
        listed
    }

    /// The files of the project named `names` (at most `limit` of each), by their file name.
    async fn find_sources(
        cx: &mut AsyncApp,
        window: String,
        names: Vec<String>,
        limit: u32,
    ) -> Result<Vec<FoundSource>, IdeError> {
        Self::on_host(cx, move |host| async move {
            let inner = Arc::clone(&host);
            host.blocking(move || {
                let mut found = Vec::new();
                for name in names {
                    let hits = ide_host::ops::nav::find_files(&inner, &window, name.clone(), limit)?;
                    let named = hits.into_iter().filter(|hit| hit.rel_path.rsplit('/').next() == Some(&name));
                    found.extend(named.map(|hit| FoundSource {
                        path: hit.path.0.to_string_lossy().into_owned(),
                        rel_path: hit.rel_path,
                    }));
                }
                Ok(found)
            })
            .await
        })
        .await
    }

    /// The texts of `paths`, in their order; a source that cannot be read is empty (it declares no
    /// package).
    async fn read_sources(cx: &mut AsyncApp, window: String, paths: Vec<String>) -> Result<Vec<String>, IdeError> {
        Self::on_host(cx, move |host| async move {
            let mut texts = Vec::with_capacity(paths.len());
            for path in paths {
                let text = ide_host::ops::fs::read_text(&host, &window, PathBuf::from(path)).await;
                texts.push(text.unwrap_or_default());
            }
            Ok(texts)
        })
        .await
    }

    /// The configuration an action argument names: a configuration id, a configuration, or
    /// `{ config, temporary: true }` (it becomes a temporary configuration, as IDEA does).
    pub fn config_from_arg(&mut self, arg: Option<&serde_json::Value>, cx: &mut Context<Self>) -> Option<RunConfig> {
        match arg? {
            serde_json::Value::String(id) => self.store.read(cx).config(id).cloned(),
            serde_json::Value::Object(fields) if fields.contains_key("config") => {
                let config: RunConfig = serde_json::from_value(fields.get("config")?.clone()).ok()?;
                if fields.get("temporary").and_then(serde_json::Value::as_bool) != Some(true) {
                    return Some(config);
                }
                let (adopted, persist) = self.store.update(cx, |store, cx| {
                    let adopted = store.adopt_temporary(config, &[]);
                    cx.notify();
                    adopted
                });
                self.carry_out_persist(persist, cx);
                Some(adopted)
            }
            other => serde_json::from_value(other.clone()).ok(),
        }
    }

    /// Advances a start with the store, the start environment, the time and the text catalog.
    fn advance<R>(
        this: &WeakEntity<Self>,
        cx: &mut AsyncApp,
        step: impl FnOnce(&mut RunStore, StartEnv, f64, &mut I18n) -> R,
    ) -> Option<R> {
        this.update(cx, |driver, cx| {
            let settings = run_settings(&driver.settings(cx));
            let types = Rc::clone(&driver.types);
            driver.store.update(cx, |store, cx| {
                let env = StartEnv { types: &types, settings: &settings };
                let i18n = &mut cx.default_global::<Localization>().0;
                let next = step(store, env, now_ms(), i18n);
                cx.notify();
                next
            })
        })
        .ok()
    }

    /// Answers the steps of `start` until it is done.
    async fn drive(this: WeakEntity<Self>, mut start: StartRun, first: StartStep, cx: &mut AsyncApp) -> Option<String> {
        let mut step = first;
        loop {
            let effects = start.take_effects();
            this.update(cx, |driver, cx| driver.apply_effects(effects, cx)).ok()?;
            step = match step {
                StartStep::Done(session) => return session,
                StartStep::Build(request) => {
                    let name = start.config().name.clone();
                    if let Some(error) = Self::build(&this, request, cx).await {
                        let mut request =
                            NotificationRequest::new(NotificationLevel::Error, format!("Cannot run '{name}'"));
                        request.group = Some("run".to_owned());
                        request.body = Some(error);
                        this.update(cx, |driver, cx| driver.notify(request, cx)).ok()?;
                    }
                    return None;
                }
                StartStep::BeforeLaunch { config, chain, show } => {
                    let ok = Self::before_launch(&this, *config, chain, show, cx).await;
                    Self::advance(&this, cx, |store, env, _, _| start.before_launch_done(ok, store, env))?
                }
                StartStep::StartChildren { configs, chain, show } => {
                    this.update(cx, |driver, cx| {
                        for config in configs {
                            let options = StartOptions { chain: chain.clone(), show, ..StartOptions::default() };
                            driver.start(config, options, cx).detach();
                        }
                    })
                    .ok()?;
                    return None;
                }
                StartStep::ConfirmRerun { name, count } => {
                    let asked = this
                        .update(cx, |driver, cx| {
                            let (title, message, ok) =
                                rerun_question(&name, count, &mut cx.default_global::<Localization>().0);
                            driver.ask(&title, &message, &ok, cx)
                        })
                        .ok()?;
                    let confirmed = match asked {
                        Some(answer) => answer.await.ok() == Some(0),
                        None => false,
                    };
                    Self::advance(&this, cx, |store, _, _, _| start.rerun_confirmed(confirmed, store))?
                }
                StartStep::StopAndWait { session_ids } => {
                    let gone = this
                        .update(cx, |driver, cx| {
                            for id in &session_ids {
                                driver.stop(id, cx);
                            }
                            driver.wait_for_exit(session_ids.clone(), cx)
                        })
                        .ok()?;
                    // No time limit: a first Stop waits for the shutdown, a second one kills it.
                    let _ = gone.await;
                    start.instances_stopped()
                }
                StartStep::SaveAll => {
                    // Saving is best effort: a file that cannot be saved does not stop the run.
                    if let Err(error) = Self::save(&this, cx).await {
                        tracing::warn!("cannot save before running: {error}");
                    }
                    Self::advance(&this, cx, |store, env, now, _| start.saved(store, env, now))?
                }
                StartStep::GradleDelegation(launch) => {
                    // The Gradle builds come from the build model, which this window does not load: the
                    // application is launched on the language server's classpath, as without a model.
                    let (_, delegated) = GradleDelegation::start(&launch, Default::default());
                    let delegated = match delegated {
                        crate::run::gradle_run::DelegationStep::Done(result) => result.map(|found| found.map(|c| *c)),
                        _ => Ok(None),
                    };
                    Self::advance(&this, cx, |store, env, now, i18n| {
                        start.delegation_resolved(delegated, store, env, now, i18n)
                    })?
                }
                StartStep::PrepareJava(first) => {
                    let outcome = Self::prepare_java(&this, &mut start, first, cx).await;
                    Self::advance(&this, cx, |store, env, now, _| start.java_prepared(outcome, store, env, now))?
                }
                StartStep::PrepareType(launch) => {
                    let prepared = Self::prepare_type(&this, *launch, cx).await;
                    Self::advance(&this, cx, |store, _, now, i18n| start.type_prepared(prepared, store, now, i18n))?
                }
                StartStep::FinalizeLaunch(launch) => {
                    let finalized = Self::finalize_launch(&this, *launch, cx).await;
                    Self::advance(&this, cx, |store, _, now, i18n| {
                        start.launch_finalized(Ok(finalized), store, now, i18n)
                    })?
                }
                StartStep::RunStart(launch) => {
                    let session = start.session_id().map(str::to_owned);
                    let generation = start.generation();
                    let started = match session {
                        Some(session) => Self::run_process(&this, session, generation, *launch, cx).await,
                        None => Err(IdeError::internal("the run has no tab")),
                    };
                    Self::advance(&this, cx, |store, _, now, i18n| start.process_started(started, store, now, i18n))?
                }
            };
        }
    }

    /// Carries out what a start asks for on the side.
    fn apply_effects(&mut self, effects: Vec<StartEffect>, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                StartEffect::Notify(request) => self.notify(request, cx),
                StartEffect::ShowRunWindow { focus } => cx.emit(RunDriverEvent::ShowRunWindow { focus }),
                // The configuration ran: it leads the chooser's recent configurations.
                StartEffect::RecordRun(id) => {
                    let write = self.store.update(cx, |store, _| store.mark_recent(&id));
                    Self::persist(write, cx);
                }
                StartEffect::Resize { run, cols, rows } => {
                    if let Some(backend) = cx.try_global::<Backend>() {
                        let resized = backend
                            .with_host(move |host| async move { ide_host::ops::run::resize(&host, run, cols, rows) });
                        cx.background_executor()
                            .spawn(async move {
                                if let Err(error) = flatten(resized.await) {
                                    tracing::debug!("cannot resize the run's terminal: {}", error.message);
                                }
                            })
                            .detach();
                    }
                }
                StartEffect::Exited { session_id, code } => self.finish_later(session_id, code, cx),
            }
        }
    }

    async fn save(this: &WeakEntity<Self>, cx: &mut AsyncApp) -> Result<(), String> {
        let saving = this
            .update(cx, |driver, cx| driver.save_all.clone().map(|save| save(cx)))
            .map_err(|error| error.to_string())?;
        match saving {
            Some(saving) => saving.await,
            None => Ok(()),
        }
    }

    /// Dispatches a build action and reads its outcome; the error, if any.
    async fn build(this: &WeakEntity<Self>, request: BuildRequest, cx: &mut AsyncApp) -> Option<String> {
        let window = this.update(cx, |driver, _| driver.window).ok().flatten();
        let registered = cx
            .update(|cx| cx.try_global::<ActionRegistry>().is_some_and(|registry| registry.contains(&request.action)));
        let Some(window) = window.filter(|_| registered) else { return Some("Building is not available".to_owned()) };
        let outcome = window.update(cx, |_, window, cx| {
            crate::platform::context::clear_context(window, cx, BUILD_LAST_STATUS);
            let executed = execute_action(&request.action, Some(&request.arg), window, cx);
            let context = crate::platform::context::window_context(window, cx);
            build_outcome(executed, &context).map(str::to_owned)
        });
        outcome.unwrap_or_else(|error| Some(error.to_string()))
    }

    /// Runs the before launch tasks of `config`; whether the launch may go on.
    async fn before_launch(
        this: &WeakEntity<Self>,
        config: RunConfig,
        chain: Vec<String>,
        show: Option<bool>,
        cx: &mut AsyncApp,
    ) -> bool {
        fn with_env<R>(
            this: &WeakEntity<RunDriver>,
            cx: &mut AsyncApp,
            act: impl FnOnce(LaunchEnv, &I18n) -> R,
        ) -> Option<R> {
            this.update(cx, |driver, cx| {
                let store = driver.store.read(cx);
                let root = store.project_root.clone();
                let env = LaunchEnv {
                    configs: store.configs(),
                    project_root: root.as_deref(),
                    modules: &[],
                    external_tools: &[],
                    tool_windows: &driver.tool_windows,
                };
                let i18n = cx.try_global::<Localization>().map(|l| &l.0);
                let fallback = I18n::default();
                act(env, i18n.unwrap_or(&fallback))
            })
            .ok()
        }
        let Some((mut launch, mut step)) =
            with_env(this, cx, |env, i18n| BeforeLaunch::start(&config, chain, show, env, i18n))
        else {
            return false;
        };
        loop {
            let next = match step {
                LaunchStep::Done { ok, notification } => {
                    if let Some(request) = notification {
                        let _ = this.update(cx, |driver, cx| driver.notify(request, cx));
                    }
                    return ok;
                }
                LaunchStep::Build(request) => {
                    let error = Self::build(this, request, cx).await;
                    with_env(this, cx, |env, i18n| launch.task_finished(error.as_deref(), env, i18n))
                }
                LaunchStep::Run { config, chain, show } => {
                    let options = StartOptions { chain, show, ..StartOptions::default() };
                    let Ok(started) = this.update(cx, |driver, cx| driver.start(*config, options, cx)) else {
                        return false;
                    };
                    let ok = match started.await {
                        Some(session) => Self::exited_cleanly(this, session, cx).await,
                        None => false,
                    };
                    with_env(this, cx, |env, i18n| launch.run_finished(ok, env, i18n))
                }
                // External tools are not listed in this window: none can be resolved.
                LaunchStep::ExternalTool { .. } => {
                    with_env(this, cx, |env, i18n| launch.tool_resolved(None, env, i18n))
                }
                LaunchStep::ShowToolWindow(id) => {
                    let _ = this.update(cx, |_, cx| cx.emit(RunDriverEvent::ShowToolWindow(id)));
                    with_env(this, cx, |env, i18n| launch.task_finished(None, env, i18n))
                }
            };
            let Some(next) = next else { return false };
            step = next;
        }
    }

    /// Waits until the session's process is gone: whether it exited with code 0 without being stopped.
    async fn exited_cleanly(this: &WeakEntity<Self>, session: String, cx: &mut AsyncApp) -> bool {
        let Ok(gone) = this.update(cx, |driver, cx| driver.wait_for_exit(vec![session.clone()], cx)) else {
            return false;
        };
        let _ = gone.await;
        this.update(cx, |driver, cx| {
            driver.store.read(cx).session(&session).is_some_and(|session| {
                session.status == SessionStatus::Finished && session.exit_code == Some(0) && !session.stopped
            })
        })
        .unwrap_or(false)
    }

    /// Drives the build and classpath preparation of a Java application to its end.
    async fn prepare_java(
        this: &WeakEntity<Self>,
        start: &mut StartRun,
        first: PrepareStep,
        cx: &mut AsyncApp,
    ) -> Result<PreparedLaunch, JavaLaunchError> {
        let ended = || JavaLaunchError::Other("The launch preparation ended".to_owned());
        let session = start.session_id().map(str::to_owned);
        let label = this.update(cx, |driver, _| driver.label.clone()).map_err(|_| ended())?;
        let mut step = first;
        loop {
            Self::advance(this, cx, |store, _, now, _| start.report_progress(store, now)).ok_or_else(ended)?;
            step = match step {
                PrepareStep::Done(prepared) => return Ok(*prepared),
                PrepareStep::Failed(error) => return Err(error),
                PrepareStep::SaveAll => {
                    let saved = Self::save(this, cx).await;
                    Self::with_i18n(cx, |i18n| start.preparation().map(|p| p.saved(saved, i18n))).ok_or_else(ended)?
                }
                PrepareStep::FindFiles { names, limit } => {
                    let found =
                        Self::find_sources(cx, label.clone(), names, limit).await.map_err(JavaLaunchError::Backend)?;
                    Self::with_i18n(cx, |i18n| start.preparation().map(|p| p.files_found(found, i18n)))
                        .ok_or_else(ended)?
                }
                PrepareStep::ListConfigs => {
                    let configs =
                        this.update(cx, |driver, cx| driver.store.read(cx).configs().to_vec()).map_err(|_| ended())?;
                    Self::with_i18n(cx, |i18n| start.preparation().map(|p| p.configs_listed(configs, i18n)))
                        .ok_or_else(ended)?
                }
                PrepareStep::ReadSources { paths } => {
                    let texts = Self::read_sources(cx, label.clone(), paths).await.map_err(JavaLaunchError::Backend)?;
                    // The build model's module directories are not loaded in this window.
                    Self::with_i18n(cx, |i18n| start.preparation().map(|p| p.sources_read(&texts, &[], i18n)))
                        .ok_or_else(ended)?
                }
                PrepareStep::ResolveLaunch { module, test_classpath, policy } => {
                    let request = Backend::next_request_id();
                    if let Some(session) = &session {
                        this.update(cx, |driver, cx| {
                            driver.store.update(cx, |store, _| store.set_resolve_request(session, Some(request)));
                        })
                        .map_err(|_| ended())?;
                    }
                    let window = label.clone();
                    let resolved = Self::on_host(cx, move |host| async move {
                        let module = PathBuf::from(module);
                        ide_host::ops::java::java_resolve_launch(
                            &host,
                            &window,
                            module,
                            test_classpath,
                            policy,
                            request,
                        )
                        .await
                    });
                    let resolved = resolved.await;
                    Self::with_i18n(cx, |i18n| start.preparation().map(|p| p.launch_resolved(resolved, i18n)))
                        .ok_or_else(ended)?
                }
            };
        }
    }

    /// The launch as its type adjusts it: a Gradle configuration's VM options go into an init script.
    async fn prepare_type(this: &WeakEntity<Self>, launch: RunConfig, cx: &mut AsyncApp) -> Result<RunConfig, String> {
        if launch.kind != "gradle" {
            return Ok(launch);
        }
        let env = TypeEnv { windows: cfg!(windows), gradle_local: false };
        let Some(jvm_args) = gradle_jvm_args_script(&launch, &env)? else { return Ok(launch) };
        let label = this.update(cx, |driver, _| driver.label.clone()).map_err(|error| error.to_string())?;
        let writing = Self::on_host(cx, move |host| async move {
            ide_host::ops::run::run_gradle_jvm_args_script(&host, &label, jvm_args).await
        });
        let script = writing.await.map_err(|error| error.message)?;
        Ok(with_jvm_args_script(&launch, &script))
    }

    /// What the process host gets: per-test reporting for a test run, and the Gradle init script a
    /// Gradle test run or BootRun needs. A script that cannot be written leaves the launch as it is.
    async fn finalize_launch(this: &WeakEntity<Self>, launch: RunConfig, cx: &mut AsyncApp) -> RunConfig {
        let launch = if launch.is_test { with_test_reporting(&launch) } else { launch };
        let script = if launch.is_test { gradle_test_script(&launch) } else { gradle_boot_script(&launch) };
        let Some(script) = script else { return launch };
        let Ok(label) = this.update(cx, |driver, _| driver.label.clone()) else { return launch };
        let writing = Self::on_host(cx, move |host| async move {
            let (name, text) = (script.name.to_owned(), script.text.to_owned());
            ide_host::ops::run::run_gradle_init_script(&host, &label, name, text).await
        });
        match writing.await {
            Ok(path) => launch_with_script(&launch, Some(&path)),
            Err(error) => {
                tracing::warn!("the Gradle init script {} was not written: {}", script.name, error.message);
                launch
            }
        }
    }

    /// Starts the process, its output streaming into the session's console.
    async fn run_process(
        this: &WeakEntity<Self>,
        session: String,
        generation: u64,
        launch: RunConfig,
        cx: &mut AsyncApp,
    ) -> Result<RunInfo, IdeError> {
        let (sender, chunks) = mpsc::unbounded::<OutputChunk>();
        let on_output: ide_host::ops::Emit<OutputChunk> = Arc::new(move |chunk| {
            // The receiver is gone only once the session's tab is.
            let _ = sender.unbounded_send(chunk);
        });
        let label = this
            .update(cx, |driver, cx| {
                driver.pump(session, generation, chunks, cx);
                driver.label.clone()
            })
            .map_err(|error| IdeError::internal(error.to_string()))?;
        Self::on_host(cx, move |host| async move { ide_host::ops::run::start(&host, &label, launch, on_output).await })
            .await
    }

    /// Feeds what arrives on `chunks` into the console of the session's start `generation`.
    fn pump(
        &mut self,
        session: String,
        generation: u64,
        mut chunks: mpsc::UnboundedReceiver<OutputChunk>,
        cx: &mut Context<Self>,
    ) {
        let id = session.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Some(first) = chunks.next().await {
                // What has arrived together is queued together.
                let mut batch = vec![first];
                while let Ok(more) = chunks.try_recv() {
                    batch.push(more);
                }
                if this.update(cx, |driver, cx| driver.output(&session, generation, &batch, cx)).is_err() {
                    break;
                }
            }
        });
        self.pumps.insert(id, task);
    }

    fn output(&mut self, session: &str, generation: u64, chunks: &[OutputChunk], cx: &mut Context<Self>) {
        let (schedule, reveal) = self.store.update(cx, |store, cx| {
            let (mut schedule, mut reveal, mut terminal) = (None, false, false);
            for chunk in chunks {
                let (kind, text) = ChunkKind::of(chunk);
                let queued = store.output(session, generation, kind, text, now_ms(), false);
                schedule = schedule.or(queued.schedule);
                reveal |= queued.reveal;
                terminal |= queued.terminal_data.is_some();
            }
            // A terminal console shows its output as it comes.
            if terminal {
                cx.notify();
            }
            (schedule, reveal)
        });
        if let Some(schedule) = schedule {
            self.schedule_flush(session, schedule, cx);
        }
        // "Show console when a message is printed": the session became the active tab.
        if reveal {
            self.store.update(cx, |_, cx| cx.notify());
            if !self.run_window_visible {
                cx.emit(RunDriverEvent::ShowRunWindow { focus: false });
            }
        }
    }

    fn schedule_flush(&mut self, session: &str, schedule: FlushSchedule, cx: &mut Context<Self>) {
        let id = session.to_owned();
        let task = match schedule {
            FlushSchedule::Frame => cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |driver, cx| driver.flush(&id, cx));
            }),
            FlushSchedule::Timeout(ms) => cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs_f64(ms.max(0.0) / 1000.0)).await;
                let _ = this.update(cx, |driver, cx| {
                    let fired = driver.store.update(cx, |store, _| store.flush_timer_fired(&id, now_ms(), false));
                    match fired {
                        Ok(flushed) => driver.flushed(&id, flushed.changed, flushed.open_browser, cx),
                        Err(next) => driver.schedule_flush(&id, next, cx),
                    }
                });
            }),
        };
        self.flushes.insert(session.to_owned(), task);
    }

    fn flush(&mut self, session: &str, cx: &mut Context<Self>) {
        let flushed = self.store.update(cx, |store, _| store.flush(session, now_ms()));
        self.flushed(session, flushed.changed, flushed.open_browser, cx);
    }

    fn flushed(&mut self, session: &str, changed: bool, open_browser: bool, cx: &mut Context<Self>) {
        if changed {
            self.store.update(cx, |_, cx| cx.notify());
        }
        if open_browser && let Some(config) = self.store.read(cx).session(session).map(|s| s.config.clone()) {
            cx.open_url(&tomcat_browser_url(&config));
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Exits

    /// A backend event for this window: a run's exit finishes its session.
    pub fn backend_event(&mut self, event: &IdeEvent, cx: &mut Context<Self>) {
        if let IdeEvent::RunExited { run, code } = event {
            self.run_exited(*run, *code, cx);
        }
    }

    pub fn run_exited(&mut self, run: RunId, code: Option<i32>, cx: &mut Context<Self>) {
        if let Some(session) = self.store.update(cx, |store, _| store.on_run_exited(run, code)) {
            self.finish_later(session, code, cx);
        }
    }

    /// Applies an exit once the output still on its way has landed.
    fn finish_later(&mut self, session: String, code: Option<i32>, cx: &mut Context<Self>) {
        let generation = self.store.read(cx).generation(&session);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs_f64(FINISH_DELAY_MS / 1000.0)).await;
            let _ = this.update(cx, |driver, cx| driver.finish(&session, generation, code, cx));
        })
        .detach();
    }

    fn finish(&mut self, session: &str, generation: Option<u64>, code: Option<i32>, cx: &mut Context<Self>) {
        let settings = self.settings(cx);
        let context = FinishContext {
            run_window_visible: self.run_window_visible,
            failed_tests: 0,
            go_root_set: !settings.languages.go.goroot.trim().is_empty(),
        };
        let finished = self.store.update(cx, |store, cx| {
            let finished = store.finish_session(session, generation, code, now_ms(), context);
            cx.notify();
            finished
        });
        if let Some(request) = finished.notification {
            self.notify(request, cx);
        }
        self.resolve_waiters(cx);
    }

    /// Resolves once the processes of `sessions` are gone.
    fn wait_for_exit(&mut self, sessions: Vec<String>, cx: &mut Context<Self>) -> oneshot::Receiver<()> {
        let (sender, gone) = oneshot::channel();
        if self.store.read(cx).any_live(&sessions) {
            self.waiters.push((sessions, sender));
        } else {
            let _ = sender.send(());
        }
        gone
    }

    fn resolve_waiters(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let (gone, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.waiters).into_iter().partition(|(sessions, _)| !store.any_live(sessions));
        self.waiters = waiting;
        for (_, sender) in gone {
            let _ = sender.send(());
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Stopping

    /// Stops a session: a launch still being prepared is cancelled, a process is asked to stop (a
    /// second Stop while it shuts down kills it).
    pub fn stop(&mut self, session: &str, cx: &mut Context<Self>) {
        let stop = self.store.update(cx, |store, cx| {
            let stop = store.stop_session(session, now_ms());
            cx.notify();
            stop
        });
        if let Some(stop) = stop {
            self.carry_out_stop(Some(session.to_owned()), stop, cx);
        }
    }

    /// Stops a process that no tab shows.
    pub fn stop_run(&mut self, run: RunId, cx: &mut Context<Self>) {
        let session = self.store.read(cx).sessions.iter().find(|s| s.run_id == Some(run)).map(|s| s.id.clone());
        match session {
            Some(session) => self.stop(&session, cx),
            None => self.carry_out_stop(None, StopAction::StopRun(run), cx),
        }
    }

    fn carry_out_stop(&mut self, session: Option<String>, stop: StopAction, cx: &mut Context<Self>) {
        match stop {
            // The session ended already; the backend drops what it was preparing.
            StopAction::CancelRequest(request) => {
                if let Some(backend) = cx.try_global::<Backend>() {
                    backend.host.cancel_request(request);
                }
                self.resolve_waiters(cx);
            }
            StopAction::StopRun(run) => {
                let Some(backend) = cx.try_global::<Backend>() else {
                    self.stop_failed(session, "the backend is not running", cx);
                    return;
                };
                let label = self.label.clone();
                let stopping =
                    backend.with_host(move |host| async move { ide_host::ops::run::stop(&host, &label, run) });
                cx.spawn(async move |this, cx| {
                    if let Err(error) = flatten(stopping.await) {
                        let _ = this.update(cx, |driver, cx| driver.stop_failed(session, &error.message, cx));
                    }
                })
                .detach();
            }
        }
    }

    fn stop_failed(&mut self, session: Option<String>, message: &str, cx: &mut Context<Self>) {
        let request = match session {
            Some(session) => self.store.update(cx, |store, cx| {
                let request = store.stop_failed(&session, message);
                cx.notify();
                request
            }),
            None => {
                let mut request = NotificationRequest::new(NotificationLevel::Error, "Cannot stop process");
                request.group = Some("run".to_owned());
                request.body = Some(message.to_owned());
                request
            }
        };
        self.notify(request, cx);
    }

    /// Closes a Run tab. A process still running is terminated once the user agrees (Settings › Run ›
    /// Confirm closing a running process).
    pub fn close_tab(&mut self, session: &str, cx: &mut Context<Self>) {
        let settings = run_settings(&self.settings(cx));
        let closing = self.store.update(cx, |store, cx| {
            let closing = store.close_session_tab(session, &settings, now_ms());
            cx.notify();
            closing
        });
        match closing {
            CloseTab::Missing => {}
            CloseTab::Closed(stop) => self.closed(session, stop, cx),
            CloseTab::Confirm { name } => {
                let (title, message, ok) = terminate_question(&name, &mut cx.default_global::<Localization>().0);
                let Some(answer) = self.ask(&title, &message, &ok, cx) else { return };
                let session = session.to_owned();
                cx.spawn(async move |this, cx| {
                    if answer.await.ok() != Some(0) {
                        return;
                    }
                    let _ = this.update(cx, |driver, cx| {
                        let stop = driver.store.update(cx, |store, cx| {
                            let stop = store.close_session(&session, now_ms());
                            cx.notify();
                            stop
                        });
                        driver.closed(&session, stop, cx);
                    });
                })
                .detach();
            }
        }
    }

    fn closed(&mut self, session: &str, stop: Option<StopAction>, cx: &mut Context<Self>) {
        self.flushes.remove(session);
        if let Some(stop) = stop {
            self.carry_out_stop(None, stop, cx);
        }
        self.resolve_waiters(cx);
    }

    /// Asks a yes/no question in the driver's window: the answer's index, 0 for `ok`.
    fn ask(&self, title: &str, message: &str, ok: &str, cx: &mut App) -> Option<oneshot::Receiver<usize>> {
        let window = self.window?;
        let cancel = cx.default_global::<Localization>().0.tx("Cancel");
        let asked = window.update(cx, |_, window, cx| {
            window.prompt(gpui_kit::PromptLevel::Warning, title, Some(message), &[ok, cancel.as_str()], cx)
        });
        match asked {
            Ok(answer) => Some(answer),
            Err(error) => {
                tracing::warn!("cannot ask {title:?}: {error}");
                None
            }
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Links

    /// Opens a file location printed by a process running in `cwd`: against the run directory, then
    /// the project root, then by name through the file index. A library class without sources (the
    /// language server is not asked here) and a missing file are told in a notification.
    pub fn open_location(&mut self, target: FileTarget, cwd: String, cx: &mut Context<Self>) {
        let root = self.store.read(cx).project_root.clone();
        let label = self.label.clone();
        cx.spawn(async move |this, cx| {
            let (mut resolver, mut step) = TargetResolver::start(target.clone(), &cwd, root.as_deref());
            let path = loop {
                step = match step {
                    ResolveStep::Done(path) => break path,
                    ResolveStep::FileExists { dir, name } => {
                        let path = PathBuf::from(dir).join(name);
                        let kind =
                            Self::on_host(
                                cx,
                                move |host| async move { ide_host::ops::fs::entry_kind(&host, path).await },
                            );
                        resolver.exists(matches!(kind.await, Ok(ide_vfs::EntryKind::File)))
                    }
                    ResolveStep::FindFiles { name, limit } => {
                        let window = label.clone();
                        let found = Self::on_host(cx, move |host| async move {
                            let inner = Arc::clone(&host);
                            host.blocking(move || ide_host::ops::nav::find_files(&inner, &window, name, limit)).await
                        });
                        let found: Option<Vec<FoundSource>> = found.await.ok().map(|hits| {
                            hits.into_iter()
                                .map(|hit| FoundSource {
                                    path: hit.path.0.to_string_lossy().into_owned(),
                                    rel_path: hit.rel_path,
                                })
                                .collect()
                        });
                        resolver.files_found(found.as_deref())
                    }
                    ResolveStep::ReadSource { path } => {
                        let window = label.clone();
                        let text = Self::on_host(cx, move |host| async move {
                            ide_host::ops::fs::read_text(&host, &window, PathBuf::from(path)).await
                        });
                        let text = text.await.ok();
                        resolver.source_read(text.as_deref())
                    }
                    // Library sources come from the language server, which this window does not ask.
                    ResolveStep::WorkspaceSymbols { .. } => resolver.symbols_found(None),
                };
            };
            let action = Self::with_i18n(cx, |i18n| open_resolved(&target, path.as_deref(), true, i18n));
            let _ = this.update(cx, |driver, cx| match action {
                OpenAction::OpenFile { path, selection, .. } => {
                    let (line, col) = selection.map_or((0, 0), |at| (at.line, at.col));
                    cx.emit(RunDriverEvent::OpenFile { path: PathBuf::from(path), line, col });
                }
                OpenAction::Notify(request) => driver.notify(request, cx),
            });
        })
        .detach();
    }

    /// Opens a URL printed in the output in the browser.
    pub fn open_url(&mut self, url: &str, cx: &mut Context<Self>) {
        cx.open_url(url);
    }

    // ---------------------------------------------------------------------------------------------
    // Console input

    /// Sends a line of console input to the session's process.
    pub fn send_input(&mut self, session: &str, line: &str, cx: &mut Context<Self>) {
        let sent = self.store.update(cx, |store, cx| {
            let sent = store.send_stdin(session, line, now_ms(), false);
            cx.notify();
            sent
        });
        let Some((write, queued)) = sent else { return };
        if let Some(schedule) = queued.schedule {
            self.schedule_flush(session, schedule, cx);
        }
        let Some(backend) = cx.try_global::<Backend>() else {
            return self.input_failed(session, "the backend is not running", cx);
        };
        let writing =
            backend.with_host(move |host| async move { ide_host::ops::run::stdin(&host, write.run, &write.data) });
        let session = session.to_owned();
        cx.spawn(async move |this, cx| {
            if let Err(error) = flatten(writing.await) {
                let _ = this.update(cx, |driver, cx| driver.input_failed(&session, &error.message, cx));
            }
        })
        .detach();
    }

    /// "Send EOF": closes the session's stdin.
    /// The terminal console of `session` fits `cols` × `rows` cells: the size is kept for a process
    /// still starting, and a running process's terminal is resized now.
    pub fn resize_terminal(&mut self, session: &str, cols: u16, rows: u16, cx: &mut Context<Self>) {
        let Some(run) = self.store.update(cx, |store, _| store.resize_terminal(session, cols, rows)) else { return };
        self.apply_effects(vec![StartEffect::Resize { run, cols, rows }], cx);
    }

    pub fn close_stdin(&mut self, session: &str, cx: &mut Context<Self>) {
        let Some(run) = self.store.read(cx).close_stdin(session) else { return };
        let Some(backend) = cx.try_global::<Backend>() else {
            return self.input_failed(session, "the backend is not running", cx);
        };
        let closing = backend.with_host(move |host| async move { ide_host::ops::run::close_stdin(&host, run) });
        let session = session.to_owned();
        cx.spawn(async move |this, cx| {
            let closed = flatten(closing.await);
            let _ = this.update(cx, |driver, cx| match closed {
                Ok(()) => driver.store.update(cx, |store, cx| {
                    store.stdin_closed(&session);
                    cx.notify();
                }),
                Err(error) => driver.input_failed(&session, &error.message, cx),
            });
        })
        .detach();
    }

    fn input_failed(&mut self, session: &str, message: &str, cx: &mut Context<Self>) {
        let queued = self.store.update(cx, |store, cx| {
            let i18n = &mut cx.default_global::<Localization>().0;
            store.stdin_failed(session, message, now_ms(), false, i18n)
        });
        if let Some(schedule) = queued.schedule {
            self.schedule_flush(session, schedule, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::configs::blank_config;
    use crate::run::store::FlushSchedule;
    use gpui_kit::{AppContext as _, TestAppContext};
    use std::cell::RefCell;
    mod proofs8_chooser_pinned;

    fn tomcat(params: &[(&str, &str)]) -> RunConfig {
        let mut config = blank_config("tomcat", "tomcat", "tomcat");
        config.command = "catalina".into();
        for (key, value) in params {
            config.extras.params.insert((*key).to_owned(), (*value).to_owned());
        }
        config
    }

    // src/features/run/session.test.tsx › Tomcat browser launch › opens the configured URL once after
    // startup (openBrowser=%s)
    #[test]
    fn opens_the_configured_url_once_after_startup() {
        let cases = [
            ("true", None, Some("http://localhost:18080/demo/")),
            ("true", Some("https://example.test/custom"), Some("https://example.test/custom")),
            ("false", None, None),
        ];
        for (open_browser, browser_url, expected) in cases {
            let mut params = vec![("openBrowser", open_browser), ("httpPort", "18080"), ("contextPath", "/demo")];
            params.extend(browser_url.map(|url| ("browserUrl", url)));
            let config = tomcat(&params);
            let mut store = RunStore::new();
            let id = store.start_sample_session(0.0);
            store.sessions[0].config = config.clone();
            let mut opened = Vec::new();
            for (i, line) in [
                "Starting service Catalina\n",
                "Server startup in [800] milliseconds\n",
                "Server startup in [900] milliseconds\n",
            ]
            .into_iter()
            .enumerate()
            {
                store.enqueue(&id, ChunkKind::Stdout, line, 100.0 * i as f64, false);
                if store.flush(&id, 100.0 * i as f64 + 1.0).open_browser {
                    opened.push(tomcat_browser_url(&config));
                }
            }
            assert_eq!(store.sessions[0].http_port, Some(18080));
            assert_eq!(opened, expected.map(str::to_owned).into_iter().collect::<Vec<_>>(), "{open_browser}");
        }
        assert_eq!(
            tomcat_browser_url(&tomcat(&[("deploymentPath", "C:\\apps\\shop.WAR")])),
            "http://localhost:8080/shop/"
        );
        assert_eq!(tomcat_browser_url(&tomcat(&[("artifactName", "ROOT")])), "http://localhost:8080/");
    }

    /// A driver over an empty store in a window of its own, and what it asked of the window.
    fn open(
        cx: &mut TestAppContext,
    ) -> (Entity<RunDriver>, Rc<RefCell<Vec<RunDriverEvent>>>, &mut gpui_kit::VisualTestContext) {
        cx.update(|cx| cx.set_global(crate::theme::ActiveTheme::default()));
        let (view, cx) = cx.add_window_view(|_, cx| crate::workbench::notifications_view::NotificationsView::new(cx));
        let handle = cx.update(|window, _| window.window_handle());
        let _ = view;
        let driver = cx.update(|_, cx| {
            let store = cx.new(|_| RunStore::new());
            cx.new(|_| RunDriver::new("w1", store).with_window(handle))
        });
        let events: Rc<RefCell<Vec<RunDriverEvent>>> = Rc::default();
        let seen = Rc::clone(&events);
        cx.update(|_, cx| {
            cx.subscribe(&driver, move |_, event: &RunDriverEvent, _| seen.borrow_mut().push(event.clone())).detach();
        });
        (driver, events, cx)
    }

    fn sample(driver: &Entity<RunDriver>, cx: &mut gpui_kit::VisualTestContext) -> String {
        driver.update(cx, |driver, cx| driver.store.update(cx, |store, _| store.start_sample_session(0.0)))
    }

    fn sessions(driver: &Entity<RunDriver>, cx: &mut gpui_kit::VisualTestContext) -> usize {
        driver.read_with(cx, |driver, cx| driver.store.read(cx).sessions.len())
    }

    // src/features/run/session.test.tsx › closing a Run tab › asks before terminating a running process and
    // keeps the tab when cancelled
    #[gpui_kit::test]
    async fn asks_before_terminating_a_running_process_and_keeps_the_tab_when_cancelled(cx: &mut TestAppContext) {
        let (driver, _, cx) = open(cx);
        let id = sample(&driver, cx);
        driver.update(cx, |driver, cx| driver.close_tab(&id, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(sessions(&driver, cx), 1);
        driver.update(cx, |driver, cx| driver.close_tab(&id, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Terminate");
        cx.run_until_parked();
        assert_eq!(sessions(&driver, cx), 0);
    }

    // src/features/run/session.test.tsx › closing a Run tab › closes a finished session without asking
    #[gpui_kit::test]
    async fn closes_a_finished_session_without_asking(cx: &mut TestAppContext) {
        let (driver, _, cx) = open(cx);
        let id = sample(&driver, cx);
        driver.update(cx, |driver, cx| {
            driver.store.update(cx, |store, _| {
                store.finish_session(&id, None, Some(0), 1.0, FinishContext::default());
            });
            driver.close_tab(&id, cx);
        });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert_eq!(sessions(&driver, cx), 0);
    }

    // Not from a spec: queued output is shown on the next turn of the executor, or once its timer fires.
    #[gpui_kit::test]
    async fn output_is_flushed_as_the_store_schedules_it(cx: &mut TestAppContext) {
        let (driver, _, cx) = open(cx);
        let id = sample(&driver, cx);
        let text = |driver: &Entity<RunDriver>, cx: &mut gpui_kit::VisualTestContext| {
            driver.read_with(cx, |driver, cx| driver.store.read(cx).console(&id).map(|c| c.text()).unwrap_or_default())
        };
        driver.update(cx, |driver, cx| {
            driver.output(&id, 0, &[OutputChunk::Stdout { text: "one\n".into() }], cx);
        });
        cx.run_until_parked();
        assert_eq!(text(&driver, cx), "one");
        // Within the flush interval the next chunk waits for a timer.
        driver.update(cx, |driver, cx| {
            let queued =
                driver.store.update(cx, |store, _| store.enqueue(&id, ChunkKind::Stderr, "two\n", now_ms(), false));
            assert!(matches!(queued.schedule, Some(FlushSchedule::Timeout(_))));
            driver.schedule_flush(&id, queued.schedule.expect("a schedule"), cx);
        });
        cx.run_until_parked();
        assert!(!text(&driver, cx).contains("two"));
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(text(&driver, cx).contains("two"), "{}", text(&driver, cx));
    }

    // Not from a spec: Stop of a launch still being prepared cancels it, and the session ends stopped;
    // a process that cannot be stopped keeps running and the failure is told.
    #[gpui_kit::test]
    async fn stopping_a_launch_being_prepared_ends_it_without_a_process(cx: &mut TestAppContext) {
        let (driver, events, cx) = open(cx);
        let id = sample(&driver, cx);
        driver.update(cx, |driver, cx| {
            driver.store.update(cx, |store, _| {
                store.sessions[0].status = SessionStatus::Starting;
                store.set_resolve_request(&id, Some(ide_core::RequestId(7)));
            });
            driver.stop(&id, cx);
        });
        cx.run_until_parked();
        let session = |driver: &Entity<RunDriver>, cx: &mut gpui_kit::VisualTestContext| {
            driver.read_with(cx, |driver, cx| driver.store.read(cx).session(&id).cloned()).expect("the tab stays")
        };
        let stopped = session(&driver, cx);
        assert_eq!((stopped.status, stopped.stopped), (SessionStatus::Finished, true));
        assert!(events.borrow().is_empty());
        driver.update(cx, |driver, cx| {
            driver.store.update(cx, |store, _| {
                store.sessions[0].status = SessionStatus::Running;
                store.sessions[0].run_id = Some(RunId(3));
            });
            driver.stop(&id, cx);
        });
        cx.run_until_parked();
        assert_eq!(session(&driver, cx).status, SessionStatus::Running);
        assert!(matches!(events.borrow().as_slice(), [RunDriverEvent::Notify(r)] if r.title == "Cannot stop process"));
    }

    /// A program configuration that runs `command arg`.
    fn program(id: &str, command: &str, arg: &str) -> RunConfig {
        RunConfig { command: command.to_owned(), args: vec![arg.to_owned()], ..blank_config(id, id, "program") }
    }

    /// The temporary folders a project test lives in: the app's home and the project.
    struct Folders {
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        root: PathBuf,
    }

    impl Folders {
        fn root_text(&self) -> String {
            self.root.to_string_lossy().into_owned()
        }

        fn saved(&self) -> Vec<String> {
            let saved = ide_run::load_user_configs(&self.root).expect("the saved configurations are read");
            saved.into_iter().map(|config| config.name).collect()
        }
    }

    /// A backend with a trusted project opened in window `w1` whose saved configurations are `configs`,
    /// and a driver of window `label` over it (`w1` drives that project, any other label has none).
    async fn project(
        cx: &mut TestAppContext,
        label: &str,
        configs: &[RunConfig],
    ) -> (Entity<RunDriver>, Rc<RefCell<Vec<RunDriverEvent>>>, Folders) {
        use crate::backend::AppDirs;
        cx.executor().allow_parking();
        let home = tempfile::tempdir().expect("a temporary home");
        let project = tempfile::tempdir().expect("a temporary project");
        let root = project.path().canonicalize().expect("the project folder exists");
        ide_run::save_user_configs(&root, configs).expect("the configurations are saved");
        cx.update(|cx| {
            cx.set_global(UiStorage::default());
            Backend::init(AppDirs::under(home.path()), cx).expect("the backend starts");
            cx.global::<Backend>().host.trust.decide(&root, true, false).expect("the project is trusted");
        });
        let open_root = root.clone();
        let opening = cx.update(|cx| {
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &open_root).await.map(|_| ())
            })
        });
        opening.await.expect("the task completes").expect("the project opens");
        let driver = cx.update(|cx| {
            let store = cx.new(|_| RunStore::new());
            cx.new(|_| RunDriver::new(label, store))
        });
        let events: Rc<RefCell<Vec<RunDriverEvent>>> = Rc::default();
        let seen = Rc::clone(&events);
        cx.update(|cx| {
            cx.subscribe(&driver, move |_, event: &RunDriverEvent, _| seen.borrow_mut().push(event.clone())).detach();
        });
        (driver, events, Folders { _home: home, _project: project, root })
    }

    /// Lists the configurations of the project and waits until they are.
    async fn listed(driver: &Entity<RunDriver>, folders: &Folders, cx: &mut TestAppContext) {
        let root = folders.root_text();
        let (sender, done) = oneshot::channel();
        driver.update(cx, |driver, cx| {
            driver.load(&root, cx);
            driver.when_loaded(
                move |_, _| {
                    let _ = sender.send(());
                },
                cx,
            );
        });
        done.await.expect("the configurations are listed");
    }

    fn ids(driver: &Entity<RunDriver>, cx: &mut TestAppContext) -> Vec<String> {
        driver.read_with(cx, |driver, cx| driver.store.read(cx).configs().iter().map(|c| c.id.clone()).collect())
    }

    fn stored(cx: &mut TestAppContext, prefix: &str, folders: &Folders) -> Option<String> {
        let key = format!("{prefix}{}", folders.root_text());
        cx.update(|cx| cx.global::<UiStorage>().get(&key).map(str::to_owned))
    }

    // Not from a spec: selecting and pinning are stored per project; deleting a saved configuration
    // removes it from the project's file and from the list read back.
    #[gpui_kit::test]
    async fn selecting_pinning_and_deleting_are_stored_and_saved(cx: &mut TestAppContext) {
        let configs = [program("hello", "/bin/echo", "hello"), program("nap", "/bin/sleep", "30")];
        let (driver, events, folders) = project(cx, "w1", &configs).await;
        listed(&driver, &folders, cx).await;
        assert_eq!(ids(&driver, cx), ["hello", "nap"]);

        driver.update(cx, |driver, cx| driver.select_config(Some("nap"), cx));
        assert_eq!(stored(cx, SELECTED_KEY, &folders).as_deref(), Some("nap"));
        driver.update(cx, |driver, cx| driver.toggle_pinned("nap", cx));
        assert_eq!(driver.read_with(cx, |driver, cx| driver.store.read(cx).pinned.clone()), ["nap"]);
        assert!(stored(cx, PINNED_KEY, &folders).is_some_and(|pinned| pinned.contains("nap")));
        driver.update(cx, |driver, cx| driver.toggle_pinned("nap", cx));
        assert_eq!(stored(cx, PINNED_KEY, &folders), None);

        let deleting = driver.update(cx, |driver, cx| driver.delete_config("hello", cx));
        assert_eq!(deleting.await, Ok(()));
        assert_eq!(folders.saved(), ["nap"]);
        assert_eq!(ids(&driver, cx), ["nap"]);
        assert!(events.borrow().is_empty(), "{:?}", events.borrow());
    }

    // Not from a spec: Save Configuration makes a temporary configuration a saved one that stays
    // selected; Edit Configurations' working copy is saved and read back.
    #[gpui_kit::test]
    async fn a_temporary_configuration_and_edited_ones_are_saved_and_listed_again(cx: &mut TestAppContext) {
        let (driver, _, folders) = project(cx, "w1", &[program("hello", "/bin/echo", "hello")]).await;
        listed(&driver, &folders, cx).await;
        driver.update(cx, |driver, cx| {
            let (_, persist) =
                driver.store.update(cx, |store, _| store.adopt_temporary(program("scratch", "/bin/echo", "x"), &[]));
            driver.carry_out_persist(persist, cx);
        });
        assert!(stored(cx, TEMPORARY_KEY, &folders).is_some_and(|temporary| temporary.contains("scratch")));

        let saving = driver.update(cx, |driver, cx| driver.save_temporary("scratch", cx));
        assert_eq!(saving.await, Ok(()));
        assert_eq!(folders.saved(), ["hello", "scratch"]);
        driver.read_with(cx, |driver, cx| {
            let store = driver.store.read(cx);
            assert!(store.temporary.is_empty());
            assert_eq!(store.selected_id.as_deref(), Some("scratch"));
        });
        assert_eq!(ids(&driver, cx), ["hello", "scratch"]);

        let mut draft = driver.read_with(cx, |driver, cx| driver.store.read(cx).configs().to_vec());
        draft[0].name = "greeting".to_owned();
        draft.pop();
        let saving = driver.update(cx, |driver, cx| driver.save_edited_configs(&draft, Vec::new(), cx));
        assert_eq!(saving.await, Ok(()));
        assert_eq!(folders.saved(), ["greeting"]);
        assert_eq!(ids(&driver, cx), ["hello"]);
    }

    // Not from a spec: a save that fails is an error and a notification, and lists nothing again.
    #[gpui_kit::test]
    async fn a_failed_save_is_an_error_and_a_notification(cx: &mut TestAppContext) {
        let (driver, events, folders) = project(cx, "w2", &[]).await;
        let saving = driver.update(cx, |driver, cx| {
            driver.save_edited_configs(&[program("hello", "/bin/echo", "hello")], Vec::new(), cx)
        });
        assert!(saving.await.is_err());
        assert!(folders.saved().is_empty());
        let notified = events
            .borrow()
            .iter()
            .any(|event| matches!(event, RunDriverEvent::Notify(r) if r.title == "Cannot save run configurations"));
        assert!(notified, "{:?}", events.borrow());
    }

    // Not from a spec: the templates are the project's own.
    #[gpui_kit::test]
    async fn templates_are_read_from_the_project(cx: &mut TestAppContext) {
        let (driver, _, folders) = project(cx, "w1", &[]).await;
        let template = RunConfig { env: [("MODE".to_owned(), "dev".to_owned())].into(), ..program("t", "", "") };
        ide_run::save_templates(&folders.root, std::slice::from_ref(&template)).expect("the template is saved");
        let loading = driver.update(cx, |driver, cx| driver.load_templates(cx));
        let templates = loading.await.expect("the templates are read");
        assert_eq!(templates.iter().map(|t| t.kind.as_str()).collect::<Vec<_>>(), ["program"]);
        let failing = cx.update(|cx| {
            let store = cx.new(|_| RunStore::new());
            cx.new(|_| RunDriver::new("w2", store))
        });
        let loading = failing.update(cx, |driver, cx| driver.load_templates(cx));
        assert!(loading.await.is_err(), "a window without a project has no templates");
    }

    // Not from a spec: Run waits for the configurations to be listed and ends with the session it
    // started; with nothing selected it opens the configuration chooser instead.
    #[gpui_kit::test]
    async fn run_waits_for_the_list_and_ends_with_the_session_it_started(cx: &mut TestAppContext) {
        let (driver, events, folders) = project(cx, "w1", &[program("hello", "/bin/echo", "hello")]).await;
        let key = format!("{SELECTED_KEY}{}", folders.root_text());
        cx.update(|cx| cx.global_mut::<UiStorage>().set(&key, "hello".to_owned()));
        let root = folders.root_text();
        let launching = driver.update(cx, |driver, cx| {
            driver.load(&root, cx);
            driver.launch_selected(cx)
        });
        let session = launching.await.expect("a session started");
        let name =
            driver.read_with(cx, |driver, cx| driver.store.read(cx).session(&session).map(|s| s.config.name.clone()));
        assert_eq!(name.as_deref(), Some("hello"));

        driver.update(cx, |driver, cx| driver.select_config(None, cx));
        let launching = driver.update(cx, |driver, cx| driver.launch_selected(cx));
        assert_eq!(launching.await, None);
        let chooser = events
            .borrow()
            .iter()
            .any(|event| matches!(event, RunDriverEvent::RunAction { id: "run.chooseConfig", arg: None }));
        assert!(chooser, "{:?}", events.borrow());
    }
}
