//! Run state: configurations, the selected configuration and run sessions (one Run tool window tab
//! each). Output goes through a per-session [`ConsoleModel`], shown in batches.
//!
//! [`RunStore`] does no I/O. A change that must reach storage returns the [`StorageWrite`]s to make, a
//! flow that needs the backend, a dialog or a timer returns its next step ([`StartStep`], [`SaveStep`],
//! [`StopAction`], [`FlushSchedule`]) and continues with the answer. Test results are not kept here:
//! every completed output line is handed back for the test tree's parsers, and the number of failed
//! tests is passed in where a notification needs it.

use crate::platform::i18n::{I18n, Param};
use crate::run::build_command::apply_build_policy_to_command;
use crate::run::build_task::BuildRequest;
use crate::run::cmake::{build_cmake, is_cmake_build};
use crate::run::config_types::RunConfigTypes;
use crate::run::configs::{blank_config, compound_children, cwd_of, param};
use crate::run::console::{ChunkKind, CompletedLine, ConsoleModel, ConsoleOptions};
use crate::run::java_launch::{
    FoundSource, JavaLaunchError, JavaPreparation, ModuleFinder, ModuleLookup, ModuleStep, PrepareStep, PreparedLaunch,
    find_java_module, is_jvm_application, parse_java_args,
};
use crate::run::links::{FileTarget, jvm_frame_target};
use crate::run::terminal_output::TerminalOutput;
use crate::workbench::notifications::{NotificationAction, NotificationLevel, NotificationRequest};
use ide_core::{IdeError, RequestId, RunId};
use ide_run::import::RunImportSource;
use ide_run::junit::{TestCaseReport, TestCaseStatus};
use ide_run::{RunConfig, RunInfo};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

/// The permanent "Current File" pseudo-configuration of the chooser (runs the active file's context).
pub const CURRENT_FILE: &str = "current-file";
/// Storage key prefixes; the project root follows.
pub const SELECTED_KEY: &str = "openidea.run.selected:";
pub const PINNED_KEY: &str = "openidea.run.pinned:";
pub const RECENT_KEY: &str = "openidea.run.recent:";
/// Temporary configurations, kept without settings-derived values.
pub const TEMPORARY_KEY: &str = "openidea.run.temporary.v2:";
pub const MAX_RECENT: usize = 5;
/// IDEA keeps at most five temporary configurations (created by gutter / context runs).
pub const MAX_TEMPORARY: usize = 5;
/// Shortest time between console updates. A line after a quiet spell shows on the next frame; a flood
/// is shown at ~20 Hz instead of every frame, which keeps typing elsewhere responsive.
pub const FLUSH_INTERVAL_MS: f64 = 50.0;
/// How long output waits while the window is hidden (frames stop firing then).
pub const HIDDEN_FLUSH_DELAY_MS: f64 = 100.0;
/// How long an exit waits before it is applied, so output still on its way lands first.
pub const FINISH_DELAY_MS: f64 = 30.0;
/// How many exits of unknown runs are remembered (background queries use the process host too).
const MAX_PENDING_EXITS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Starting,
    Running,
    Stopping,
    Finished,
    Failed,
}

impl SessionStatus {
    /// Whether the process is alive: starting, running, or shutting down after Stop.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunSession {
    pub id: String,
    pub config: RunConfig,
    pub run_id: Option<RunId>,
    pub status: SessionStatus,
    pub exit_code: Option<i32>,
    /// Unix milliseconds.
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub stdin_closed: bool,
    /// Stopped by the user; a non-zero exit code then isn't a failure.
    pub stopped: bool,
    /// The unfiltered configuration when this run was started by "Rerun Failed Tests".
    pub base_config: Option<RunConfig>,
    /// "Emulate terminal in output console": the output is a terminal.
    pub terminal: bool,
    /// The HTTP port the application's embedded web server reported it started on.
    pub http_port: Option<u16>,
}

impl RunSession {
    pub fn is_live(&self) -> bool {
        self.status.is_live()
    }

    /// Whether the console takes typed input: the process runs and its stdin is open.
    pub fn accepts_input(&self) -> bool {
        self.status == SessionStatus::Running && self.run_id.is_some() && !self.stdin_closed
    }
}

/// What the chooser does with a configuration: select it (title bar widget), or run / debug it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChooserMode {
    #[default]
    Select,
    Run,
    Debug,
}

/// A value to store under `key`; `None` removes the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageWrite {
    pub key: String,
    pub value: Option<String>,
}

/// Temporary configurations to rewrite without what settings derived (the project's Maven, JDK,
/// interpreter…), so a later settings change reaches them. Ask the backend to strip `configs`, then
/// call [`RunStore::derived_stripped`].
#[derive(Clone, Debug, PartialEq)]
pub struct StripDerived {
    pub generation: u64,
    pub root: String,
    pub configs: Vec<RunConfig>,
}

/// What a change to the configurations leaves to do outside the store.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Persist {
    pub writes: Vec<StorageWrite>,
    pub strip_derived: Option<StripDerived>,
}

impl Persist {
    fn merge(&mut self, other: Persist) {
        self.writes.extend(other.writes);
        if other.strip_derived.is_some() {
            self.strip_derived = other.strip_derived;
        }
    }
}

/// When queued output is shown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlushSchedule {
    /// On the next frame: call [`RunStore::flush`].
    Frame,
    /// After this many milliseconds: call [`RunStore::flush_timer_fired`].
    Timeout(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlushHandle {
    Frame,
    /// Waiting out the hidden window: flushes when it fires.
    HiddenTimeout,
    /// Waiting out [`FLUSH_INTERVAL_MS`]: flushes on the frame after it fires.
    IntervalTimeout,
}

/// What became of a chunk of output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Enqueued {
    /// Schedule a flush (none is pending).
    pub schedule: Option<FlushSchedule>,
    /// Data for the session's terminal, when it emulates one.
    pub terminal_data: Option<String>,
    /// "Show console when a message is printed": the session became the active tab; show the Run
    /// tool window if it is hidden.
    pub reveal: bool,
}

/// What a flush showed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// The console changed (its version was bumped).
    pub changed: bool,
    /// Process output lines completed by this flush, for line listeners and test parsers.
    pub lines: Vec<CompletedLine>,
    /// A Tomcat configuration with "Open browser after launch" finished starting.
    pub open_browser: bool,
}

/// What stopping a session asks of the backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopAction {
    /// The launch was still being prepared: cancel that request.
    CancelRequest(RequestId),
    /// Stop the process; on failure call [`RunStore::stop_failed`].
    StopRun(RunId),
}

/// Data to write to a process's stdin; on failure call [`RunStore::stdin_failed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdinWrite {
    pub run: RunId,
    pub data: String,
}

/// What closing a Run tab needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseTab {
    Missing,
    /// The process still runs: ask whether to terminate `name`, then [`RunStore::close_session`].
    Confirm {
        name: String,
    },
    Closed(Option<StopAction>),
}

/// Run settings the store reads (Settings › Run, and the run targets chosen per language).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSettings {
    pub confirm_rerun: bool,
    pub confirm_terminate: bool,
    pub show_on_start: bool,
    /// Run target of configurations that choose none; empty: this machine.
    pub default_target: String,
    /// Run target of PHP tools (`local`: this machine whatever the default is).
    pub php_target: String,
    /// Run target of Node.js tools.
    pub node_target: String,
}

impl Default for RunSettings {
    fn default() -> Self {
        Self {
            confirm_rerun: true,
            confirm_terminate: true,
            show_on_start: true,
            default_target: String::new(),
            php_target: String::new(),
            node_target: String::new(),
        }
    }
}

/// `config` on the run target the settings choose for its tool, unless it names one itself.
pub fn with_run_target(config: &RunConfig, settings: &RunSettings) -> RunConfig {
    if config.extras.target.is_some() {
        return config.clone();
    }
    let name = config.command.rsplit(['/', '\\']).next().unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let name = match [".exe", ".cmd", ".bat"].iter().find(|suffix| lower.ends_with(**suffix)) {
        Some(suffix) => &name[..name.len() - suffix.len()],
        None => name,
    };
    let kind = config.kind.as_str();
    let php = matches!(name, "php" | "composer" | "phpunit" | "pest") || matches!(kind, "php" | "composer" | "phpunit");
    let selected = if php {
        settings.php_target.as_str()
    } else if matches!(name, "node" | "npm" | "npx" | "pnpm" | "yarn" | "bun")
        || matches!(kind, "node" | "npm" | "vitest" | "jest" | "mocha")
    {
        settings.node_target.as_str()
    } else {
        ""
    };
    let mut next = config.clone();
    if selected == "local" {
        next.extras.target = Some(String::new());
        return next;
    }
    let target = if selected.is_empty() { settings.default_target.as_str() } else { selected };
    if !target.is_empty() {
        next.extras.target = Some(target.to_string());
    }
    next
}

/// IDEA picks an application config first; prefer a run/dev/start target over builds and tests.
pub fn default_selection(configs: &[RunConfig]) -> Option<&RunConfig> {
    static RUNS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"bootrun|spring-boot:run|:run(?-u:\b)|run:|(?-u:\b)dev(?-u:\b)|(?-u:\b)start(?-u:\b)",
            r"|(?-u:\b)serve(?-u:\b)|runserver",
        ))
        .expect("the run target pattern is valid")
    });
    static BUILDS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("build|package|lint|check|format|clean").expect("the build pattern is valid"));
    let score = |config: &RunConfig| {
        if config.is_test {
            return 1;
        }
        if config.kind == "spring-boot" {
            return 4;
        }
        let text = format!("{} {}", config.id, config.name).to_lowercase();
        if RUNS.is_match(&text) {
            3
        } else if BUILDS.is_match(&text) {
            0
        } else {
            2
        }
    };
    // The first of the best, as a stable sort by score would put it first.
    let mut best: Option<(&RunConfig, i32)> = None;
    for config in configs {
        let points = score(config);
        if best.is_none_or(|(_, top)| points > top) {
            best = Some((config, points));
        }
    }
    best.map(|(config, _)| config)
}

/// Ids stored as a JSON array of strings; anything else reads as none.
pub fn parse_ids(stored: Option<&str>) -> Vec<String> {
    let Some(serde_json::Value::Array(items)) = stored.and_then(|raw| serde_json::from_str(raw).ok()) else {
        return Vec::new();
    };
    items.into_iter().filter_map(|item| item.as_str().map(str::to_string)).collect()
}

/// Stored temporary configurations; entries that are not configurations are skipped.
pub fn parse_temporary(stored: Option<&str>) -> Vec<RunConfig> {
    let Some(serde_json::Value::Array(items)) = stored.and_then(|raw| serde_json::from_str(raw).ok()) else {
        return Vec::new();
    };
    items.into_iter().filter_map(|item| serde_json::from_value(item).ok()).collect()
}

fn write_ids(prefix: &str, root: &str, ids: &[String]) -> StorageWrite {
    let value = if ids.is_empty() { None } else { serde_json::to_string(ids).ok() };
    StorageWrite { key: format!("{prefix}{root}"), value }
}

fn write_temporary(root: &str, configs: &[RunConfig]) -> StorageWrite {
    let value = if configs.is_empty() { None } else { serde_json::to_string(configs).ok() };
    StorageWrite { key: format!("{TEMPORARY_KEY}{root}"), value }
}

/// IDEA's `isConfigurationFromContext` for an Application: the same main class of the same module.
fn same_application(a: &RunConfig, b: &RunConfig) -> bool {
    if !is_jvm_application(&a.kind) || !is_jvm_application(&b.kind) {
        return false;
    }
    fn owner(config: &RunConfig) -> Option<&str> {
        config.extras.module.as_deref().or_else(|| cwd_of(config))
    }
    let main = parse_java_args(&a.args).main_class;
    main.is_some() && main == parse_java_args(&b.args).main_class && owner(a) == owner(b)
}

/// The HTTP port of a running Spring Boot application, from the embedded web server's startup line:
/// "Tomcat started on port 8080 (http) with context path '/'", "Netty started on port 8080",
/// "Undertow started on port(s) 8080 (http)", "Jetty started on port(s) 8080 (http/1.1)".
pub fn started_port(line: &str) -> Option<u16> {
    static STARTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)(?-u:\b)(?:Tomcat|Netty|Undertow|Jetty)(?-u:\b).*?",
            r"(?-u:\b)started on port(?:\(s\))?:?\s*([0-9]{1,5})",
        ))
        .expect("the server port pattern is valid")
    });
    if !line.contains("tarted on port") {
        return None;
    }
    STARTED.captures(line)?.get(1)?.as_str().parse::<u16>().ok().filter(|port| *port > 0)
}

/// A test tree event read from a JUnit XML report.
#[derive(Clone, Debug, PartialEq)]
pub enum ReportEvent {
    Result { path: Vec<String>, status: TestCaseStatus, duration_ms: Option<f64> },
    Output { path: Vec<String>, line: String },
    Location { path: Vec<String>, location: FileTarget },
}

/// JUnit XML report cases → test tree events (`[class, method]`, failure output and source line).
pub fn test_report_events(reports: &[TestCaseReport]) -> Vec<ReportEvent> {
    let mut events = Vec::new();
    for report in reports {
        if report.class_name.is_empty() || report.name.is_empty() {
            continue;
        }
        let path = vec![report.class_name.clone(), report.name.clone()];
        let (status, duration_ms) = (report.status, report.duration_ms);
        events.push(ReportEvent::Result { path: path.clone(), status, duration_ms });
        if report.status != TestCaseStatus::Failed {
            continue;
        }
        let parts = [report.message.as_deref(), report.output.as_deref()];
        let text = parts.into_iter().flatten().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
        for line in text.split('\n').take(200) {
            events.push(ReportEvent::Output { path: path.clone(), line: line.to_string() });
        }
        // The frame of the test class itself (or one nested in it), behind an optional class loader.
        let outer = regex::escape(report.class_name.split('$').next().unwrap_or_default());
        let frame = Regex::new(&format!(
            concat!(
                r"at (?:\S*/)?({}(?:\$[A-Za-z0-9_$]+)?)\.[^()\n]+",
                r"\(([A-Za-z0-9_$-]+\.(?:java|kt|groovy|scala)):([0-9]+)\)",
            ),
            outer
        ));
        let found = frame.ok().and_then(|frame| {
            let found = frame.captures(report.output.as_deref().unwrap_or_default())?;
            Some(jvm_frame_target(Some(found.get(1)?.as_str()), found.get(2)?.as_str(), found[3].parse().ok()))
        });
        if let Some(location) = found {
            events.push(ReportEvent::Location { path, location });
        }
    }
    events
}

#[derive(Clone, Debug)]
struct SessionRuntime {
    console: ConsoleModel,
    queue: Vec<(ChunkKind, String)>,
    flush_handle: Option<FlushHandle>,
    /// When output was last shown.
    flushed_at: f64,
    /// Bumped per start so output from a previous run in the same tab is ignored.
    generation: u64,
    /// User input typed into the console, not yet sent (IDEA: Enter sends the line).
    input: String,
    /// Output of a run that emulates a terminal (instead of `console`).
    terminal: Option<TerminalOutput>,
    /// Last size the terminal console reported (sent once the process has an id).
    terminal_size: Option<(u16, u16)>,
    /// The cancellable request preparing the launch.
    resolve_request: Option<RequestId>,
    /// Stop cancelled the launch while it was being prepared.
    resolve_aborted: bool,
    browser_opened: bool,
}

impl SessionRuntime {
    fn new(max_chars: Option<usize>, generation: u64) -> Self {
        let options = ConsoleOptions { max_chars, report_lines: true, ..ConsoleOptions::default() };
        Self {
            console: ConsoleModel::new(options),
            queue: Vec::new(),
            flush_handle: None,
            flushed_at: f64::NEG_INFINITY,
            generation,
            input: String::new(),
            terminal: None,
            terminal_size: None,
            resolve_request: None,
            resolve_aborted: false,
            browser_opened: false,
        }
    }
}

/// What applying an exit needs to know from outside the store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinishContext {
    /// The Run tool window is showing (its tab tells the outcome; no notification then).
    pub run_window_visible: bool,
    /// Failed tests of the run, from the test tree.
    pub failed_tests: usize,
    /// Settings name a Go SDK.
    pub go_root_set: bool,
}

/// What an applied exit leaves to do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Finished {
    /// Lines completed by the final flush.
    pub lines: Vec<CompletedLine>,
    pub notification: Option<NotificationRequest>,
    /// The exit code and whether the user stopped the run, for whoever started it; `None` when the
    /// session is gone or was restarted meanwhile.
    pub exited: Option<(Option<i32>, bool)>,
}

/// Run configurations, the chooser's state and the Run tool window's sessions of one project window.
#[derive(Clone, Debug, Default)]
pub struct RunStore {
    configs: Vec<RunConfig>,
    pub loaded: bool,
    pub load_error: Option<String>,
    pub selected_id: Option<String>,
    /// Ids of temporary configurations (created by gutter / context runs, not saved; last in `configs`).
    pub temporary: Vec<String>,
    /// Ids of configurations pinned to the top of the chooser, in pin order.
    pub pinned: Vec<String>,
    /// Ids of the configurations started last, newest first (at most [`MAX_RECENT`]).
    pub recent: Vec<String>,
    pub sessions: Vec<RunSession>,
    pub active_session: Option<String>,
    /// Output version per session (redraw trigger for the console).
    pub versions: BTreeMap<String, u64>,
    pub chooser_open: bool,
    pub chooser_mode: ChooserMode,
    pub editor_open: bool,
    /// Config to preselect when the edit dialog opens.
    pub edit_target: Option<String>,
    /// The edit dialog opens with unsaved changes (a temporary configuration to make permanent on OK).
    pub edit_dirty: bool,
    /// The edit dialog opens by importing from this project file.
    pub edit_import: Option<RunImportSource>,
    /// Root of the open project; storage keys end with it.
    pub project_root: Option<String>,
    /// Settings › Console › Override console cycle buffer size, in characters.
    pub console_char_limit: Option<usize>,
    runtimes: HashMap<String, SessionRuntime>,
    session_seq: u64,
    /// Counts rewrites of the stored temporary configurations; a late answer of an earlier one is dropped.
    persisting: u64,
    /// Exit codes that arrived before the start call returned the run's id.
    pending_exits: Vec<(RunId, Option<i32>)>,
    /// Ids of every configuration listed in this window, so a run of one deleted since can be told apart.
    listed: HashSet<String>,
    /// The temporary configuration "Modify Run Configuration…" opened the edit dialog on.
    modifying: Option<String>,
}

/// The user's configurations to save after a deletion, and the detected ids to suppress.
pub type SavedConfigs = (Vec<RunConfig>, Vec<String>);

impl RunStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn configs(&self) -> &[RunConfig] {
        &self.configs
    }

    pub fn set_configs(&mut self, configs: Vec<RunConfig>) {
        self.listed.extend(configs.iter().map(|config| config.id.clone()));
        self.configs = configs;
    }

    pub fn config(&self, id: &str) -> Option<&RunConfig> {
        self.configs.iter().find(|config| config.id == id)
    }

    fn is_temporary(&self, id: &str) -> bool {
        self.temporary.iter().any(|temporary| temporary == id)
    }

    // -----------------------------------------------------------------------------------------------
    // Configurations

    /// No project is open: nothing is listed.
    pub fn unloaded(&mut self) {
        self.set_configs(Vec::new());
        self.loaded = false;
        self.selected_id = None;
    }

    /// The stored temporary configurations that survive a reload: a saved one with the same id wins.
    /// They are kept without what settings derived, so pass them through the backend's settings before
    /// [`Self::loaded`].
    pub fn kept_temporary(saved: &[RunConfig], stored: Option<&str>) -> Vec<RunConfig> {
        let is_saved = |id: &str| saved.iter().any(|config| config.id == id);
        parse_temporary(stored).into_iter().filter(|config| !is_saved(&config.id)).collect()
    }

    /// The configurations were read: `saved` from the backend, `temporary` from [`Self::kept_temporary`],
    /// and the stored selection, pinned and recent ids of this project.
    pub fn loaded(
        &mut self,
        saved: Vec<RunConfig>,
        temporary: Vec<RunConfig>,
        stored_selected: Option<&str>,
        stored_pinned: Option<&str>,
        stored_recent: Option<&str>,
    ) {
        let stored = self.selected_id.clone().or_else(|| stored_selected.map(str::to_string));
        let listed = |id: &str| saved.iter().chain(&temporary).any(|config| config.id == id);
        let selected = match stored {
            Some(id) if id == CURRENT_FILE || listed(&id) => id,
            _ => default_selection(&saved).map_or(CURRENT_FILE.to_string(), |config| config.id.clone()),
        };
        self.temporary = temporary.iter().map(|config| config.id.clone()).collect();
        self.set_configs(saved.into_iter().chain(temporary).collect());
        self.pinned = parse_ids(stored_pinned);
        self.recent = parse_ids(stored_recent);
        self.loaded = true;
        self.load_error = None;
        self.selected_id = Some(selected);
    }

    pub fn load_failed(&mut self, message: &str) {
        self.loaded = true;
        self.load_error = Some(message.to_string());
    }

    pub fn select_config(&mut self, id: Option<&str>) -> Option<StorageWrite> {
        self.selected_id = id.map(str::to_string);
        let root = self.project_root.as_deref()?;
        let value = id.filter(|id| !id.is_empty()).map(str::to_string);
        Some(StorageWrite { key: format!("{SELECTED_KEY}{root}"), value })
    }

    /// Pins a configuration to the top of the chooser, or unpins it. Kept per project.
    pub fn toggle_pinned(&mut self, id: &str) -> Option<StorageWrite> {
        match self.pinned.iter().position(|pinned| pinned == id) {
            Some(index) => {
                self.pinned.remove(index);
            }
            None => self.pinned.push(id.to_string()),
        }
        self.project_root.as_deref().map(|root| write_ids(PINNED_KEY, root, &self.pinned))
    }

    /// A configuration was started: it leads the chooser's recent configurations. Kept per project.
    pub fn mark_recent(&mut self, id: &str) -> Option<StorageWrite> {
        if id == CURRENT_FILE || self.recent.first().is_some_and(|first| first == id) {
            return None;
        }
        self.recent.retain(|recent| recent != id);
        self.recent.insert(0, id.to_string());
        self.recent.truncate(MAX_RECENT);
        self.project_root.as_deref().map(|root| write_ids(RECENT_KEY, root, &self.recent))
    }

    /// Whether the chooser's "Current File" is selected.
    pub fn current_file_selected(&self) -> bool {
        self.selected_id.as_deref() == Some(CURRENT_FILE)
    }

    pub fn selected_config(&self) -> Option<&RunConfig> {
        self.config(self.selected_id.as_deref()?)
    }

    /// Stores the temporary configurations as they are now.
    pub fn persist_temporary(&mut self) -> Persist {
        let Some(root) = self.project_root.clone() else { return Persist::default() };
        let list: Vec<RunConfig> = self.temporary.iter().filter_map(|id| self.config(id).cloned()).collect();
        let write = write_temporary(&root, &list);
        // A rewrite still pending from an earlier call must not bring back what was removed since.
        self.persisting += 1;
        let strip_derived =
            (!list.is_empty()).then_some(StripDerived { generation: self.persisting, root, configs: list });
        Persist { writes: vec![write], strip_derived }
    }

    /// The backend stripped the derived values of a [`StripDerived`]: what to store, unless the
    /// temporary configurations or the project changed meanwhile.
    pub fn derived_stripped(&self, generation: u64, root: &str, kept: &[RunConfig]) -> Option<StorageWrite> {
        let current = generation == self.persisting && self.project_root.as_deref() == Some(root);
        current.then(|| write_temporary(root, kept))
    }

    /// Selects the configuration for a gutter / context run: a saved or detected configuration with
    /// the same id is reused, otherwise `config` becomes a temporary one (the oldest idle temporary is
    /// dropped beyond five). `debugging`: ids of the configurations with a debug session.
    pub fn adopt_temporary(&mut self, config: RunConfig, debugging: &[String]) -> (RunConfig, Persist) {
        // IDEA's `findExistingConfiguration`: a configuration the user keeps for this context wins.
        let existing = self
            .config(&config.id)
            .or_else(|| self.configs.iter().find(|own| !self.is_temporary(&own.id) && same_application(own, &config)));
        if let Some(existing) = existing.cloned().filter(|_| !self.is_temporary(&config.id)) {
            let writes = self.select_config(Some(&existing.id)).into_iter().collect();
            return (existing, Persist { writes, strip_derived: None });
        }
        let mut temporary: Vec<String> = self.temporary.iter().filter(|id| **id != config.id).cloned().collect();
        temporary.push(config.id.clone());
        let busy: HashSet<&str> = self
            .sessions
            .iter()
            .filter(|session| session.is_live())
            .map(|session| session.config.id.as_str())
            .chain(debugging.iter().map(String::as_str))
            .collect();
        while temporary.len() > MAX_TEMPORARY {
            let evict = temporary.iter().position(|id| *id != config.id && !busy.contains(id.as_str())).unwrap_or(0);
            temporary.remove(evict);
        }
        let kept =
            |own: &RunConfig| own.id != config.id && (!self.is_temporary(&own.id) || temporary.contains(&own.id));
        let mut configs: Vec<RunConfig> = self.configs.iter().filter(|own| kept(own)).cloned().collect();
        configs.push(config.clone());
        self.set_configs(configs);
        self.temporary = temporary;
        let mut persist = self.persist_temporary();
        persist.writes.extend(self.select_config(Some(&config.id)));
        (config, persist)
    }

    /// A file removed (or moved away) takes the temporary context configurations its path produced
    /// (`context:file:` and the compiled `context:build:` partner), like IDEA deleting the context
    /// configuration with the file. Saved and detected configurations the user made stay.
    pub fn drop_stale_context_configs(&mut self, removed: &[String]) -> Persist {
        static CONTEXT_ID: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^context:(?:file|build):(.+?)(?:#.+)?$").expect("the context id pattern is valid")
        });
        let Some(root) = self.project_root.clone() else { return Persist::default() };
        if removed.is_empty() || self.temporary.is_empty() {
            return Persist::default();
        }
        // Temporary ids carry the project-relative path (the full path outside the root).
        let prefix = format!("{root}/");
        let gone: HashSet<&str> =
            removed.iter().map(|path| path.strip_prefix(&prefix).unwrap_or(path.as_str())).collect();
        let stale = |id: &str| {
            CONTEXT_ID.captures(id).and_then(|found| found.get(1)).is_some_and(|path| gone.contains(path.as_str()))
        };
        let dropped: Vec<String> = self.temporary.iter().filter(|id| stale(id)).cloned().collect();
        if dropped.is_empty() {
            return Persist::default();
        }
        let configs = self.configs.iter().filter(|config| !dropped.contains(&config.id)).cloned().collect();
        self.set_configs(configs);
        self.temporary.retain(|id| !dropped.contains(id));
        let mut persist = self.persist_temporary();
        if self.selected_id.as_ref().is_some_and(|selected| dropped.contains(selected)) {
            persist.writes.extend(self.select_config(None));
        }
        persist
    }

    /// The user configurations: everything neither detected nor temporary.
    fn user_configs(&self) -> impl Iterator<Item = &RunConfig> {
        self.configs.iter().filter(|config| !config.detected && !self.is_temporary(&config.id))
    }

    /// "Save Configuration": a temporary configuration becomes a user configuration. Returns the user
    /// configurations to save ([`SaveConfigs`]); once saved and reloaded, select `id` again.
    pub fn save_temporary(&mut self, id: &str) -> Option<(Persist, Vec<RunConfig>)> {
        let config = self.config(id).filter(|_| self.is_temporary(id))?.clone();
        let mut user: Vec<RunConfig> = self.user_configs().cloned().collect();
        user.push(RunConfig { detected: false, ..config });
        self.temporary.retain(|temporary| temporary != id);
        Some((self.persist_temporary(), user))
    }

    /// Whether the chooser's Delete may remove `id`.
    pub fn can_delete_config(&self, id: &str) -> bool {
        self.config(id).is_some()
    }

    /// Deletes a configuration. A temporary one is forgotten; otherwise the returned user
    /// configurations are saved, with the id to suppress when the configuration was a detected one.
    pub fn delete_config(&mut self, id: &str) -> (Persist, Option<SavedConfigs>) {
        let Some(config) = self.config(id).cloned() else { return (Persist::default(), None) };
        let mut persist = Persist::default();
        if self.selected_id.as_deref() == Some(id) {
            persist.writes.extend(self.select_config(None));
        }
        if self.is_temporary(id) {
            let configs = self.configs.iter().filter(|own| own.id != id).cloned().collect();
            self.set_configs(configs);
            self.temporary.retain(|temporary| temporary != id);
            persist.merge(self.persist_temporary());
            return (persist, None);
        }
        let user = self.user_configs().filter(|own| own.id != id).cloned().collect();
        let removed = if config.detected { vec![id.to_string()] } else { Vec::new() };
        (persist, Some((user, removed)))
    }

    /// Edit Configurations' OK / Apply. A temporary configuration stays temporary unless it was edited
    /// there (then it is saved, as IDEA makes it permanent), and one removed there is forgotten.
    /// Returns the user configurations to save.
    pub fn save_edited_configs(&mut self, draft: &[RunConfig]) -> (Persist, Vec<RunConfig>) {
        let unchanged = |edited: &RunConfig| self.config(&edited.id).is_some_and(|before| before == edited);
        let kept: Vec<String> = self
            .temporary
            .iter()
            .filter(|id| draft.iter().any(|edited| edited.id == **id && unchanged(edited)))
            .cloned()
            .collect();
        let mut persist = Persist::default();
        if kept.len() != self.temporary.len() {
            let stays = |config: &RunConfig| !self.is_temporary(&config.id) || kept.contains(&config.id);
            let configs = self.configs.iter().filter(|config| stays(config)).cloned().collect();
            self.set_configs(configs);
            self.temporary = kept.clone();
            persist = self.persist_temporary();
        }
        let user = draft.iter().filter(|edited| !edited.detected && !kept.contains(&edited.id)).cloned().collect();
        (persist, user)
    }

    /// "Modify Run Configuration…": `config` becomes the selected temporary configuration and the edit
    /// dialog opens on it with pending changes, so OK saves it as a permanent one. Cancel leaves it
    /// temporary (see [`Self::editor_closed`]).
    pub fn modify_temporary(&mut self, config: RunConfig, debugging: &[String]) -> Persist {
        let (adopted, persist) = self.adopt_temporary(RunConfig { detected: false, ..config }, debugging);
        self.editor_open = true;
        self.chooser_open = false;
        self.edit_dirty = !adopted.detected;
        self.edit_target = Some(adopted.id.clone());
        self.modifying = Some(adopted.id);
        persist
    }

    /// The edit dialog closed. A configuration opened by [`Self::modify_temporary`] and not saved goes
    /// back to an ordinary (detected) temporary configuration.
    pub fn editor_closed(&mut self) -> Persist {
        self.editor_open = false;
        let Some(id) = self.modifying.take() else { return Persist::default() };
        self.edit_dirty = false;
        if self.is_temporary(&id) {
            let mut configs = self.configs.clone();
            for config in configs.iter_mut().filter(|config| config.id == id) {
                config.detected = true;
            }
            self.set_configs(configs);
        }
        self.persist_temporary()
    }

    // -----------------------------------------------------------------------------------------------
    // Sessions

    pub fn session(&self, id: &str) -> Option<&RunSession> {
        self.sessions.iter().find(|session| session.id == id)
    }

    fn session_mut(&mut self, id: &str) -> Option<&mut RunSession> {
        self.sessions.iter_mut().find(|session| session.id == id)
    }

    /// Sessions of `config`: its own, or for a compound those of its configurations.
    pub fn sessions_of(&self, config: &RunConfig) -> Vec<&RunSession> {
        if config.kind != "compound" {
            return self.sessions.iter().filter(|session| session.config.id == config.id).collect();
        }
        let children = compound_children(config, &self.configs);
        self.sessions.iter().filter(|session| children.iter().any(|child| child.id == session.config.id)).collect()
    }

    pub fn running_sessions(&self) -> Vec<&RunSession> {
        self.sessions.iter().filter(|session| session.is_live()).collect()
    }

    /// Whether any of the sessions still has a live process (what waiting for their exit watches).
    pub fn any_live(&self, session_ids: &[String]) -> bool {
        let live = |id: &String| self.session(id).is_some_and(|session| session.run_id.is_some() && session.is_live());
        session_ids.iter().any(live)
    }

    /// What rerunning a session launches: its configuration as saved now (edited since, as IDEA), or
    /// the one it ran with when that was deleted or never saved. A derived run (Rerun Failed Tests)
    /// keeps its tests.
    pub fn rerun_config<'a>(&'a self, session: &'a RunSession) -> &'a RunConfig {
        if session.base_config.is_some() {
            return &session.config;
        }
        self.config(&session.config.id).unwrap_or(&session.config)
    }

    /// The session's original configuration, as saved now (see [`Self::rerun_config`]).
    pub fn base_config_of<'a>(&'a self, session: &'a RunSession) -> &'a RunConfig {
        let base = session.base_config.as_ref().unwrap_or(&session.config);
        self.config(&base.id).unwrap_or(base)
    }

    /// The configuration was saved or temporary in this window and has been removed since.
    pub fn config_removed(&self, id: &str) -> bool {
        self.listed.contains(id) && self.config(id).is_none()
    }

    pub fn console(&self, session_id: &str) -> Option<&ConsoleModel> {
        self.runtimes.get(session_id).map(|runtime| &runtime.console)
    }

    pub fn terminal(&self, session_id: &str) -> Option<&TerminalOutput> {
        self.runtimes.get(session_id).and_then(|runtime| runtime.terminal.as_ref())
    }

    /// The typed, unsent console input of a session.
    pub fn pending_input(&self, session_id: &str) -> &str {
        self.runtimes.get(session_id).map_or("", |runtime| runtime.input.as_str())
    }

    /// Which start of the session's tab is current; output of an earlier one is ignored.
    pub fn generation(&self, session_id: &str) -> Option<u64> {
        self.runtimes.get(session_id).map(|runtime| runtime.generation)
    }

    fn bump(&mut self, session_id: &str) {
        *self.versions.entry(session_id.to_string()).or_default() += 1;
    }

    /// Queues output of the session's current start. `hidden`: the window is not showing.
    pub fn output(
        &mut self,
        session_id: &str,
        generation: u64,
        kind: ChunkKind,
        text: &str,
        now_ms: f64,
        hidden: bool,
    ) -> Enqueued {
        if self.generation(session_id) != Some(generation) {
            return Enqueued::default();
        }
        let mut enqueued = self.enqueue(session_id, kind, text, now_ms, hidden);
        let reveals = self.session(session_id).is_some_and(|session| match kind {
            ChunkKind::Stdout => session.config.extras.show_on_stdout,
            ChunkKind::Stderr => session.config.extras.show_on_stderr,
            ChunkKind::System => false,
        });
        if reveals {
            self.active_session = Some(session_id.to_string());
            enqueued.reveal = true;
        }
        enqueued
    }

    /// Queues output whatever started it (the IDE's own messages, echoed input).
    pub fn enqueue(&mut self, session_id: &str, kind: ChunkKind, text: &str, now_ms: f64, hidden: bool) -> Enqueued {
        let Some(runtime) = self.runtimes.get_mut(session_id) else { return Enqueued::default() };
        if let Some(terminal) = &mut runtime.terminal {
            let terminal_data = terminal.append(kind, text);
            // The terminal console shows the output at once: it has no flush to wait for.
            if terminal_data.is_some() {
                self.bump(session_id);
            }
            return Enqueued { terminal_data, ..Enqueued::default() };
        }
        runtime.queue.push((kind, text.to_string()));
        if runtime.flush_handle.is_some() {
            return Enqueued::default();
        }
        // Frames pause while the window is hidden; a timeout keeps consuming output so it doesn't pile
        // up until the console is shown again.
        let wait = FLUSH_INTERVAL_MS - (now_ms - runtime.flushed_at);
        let (handle, schedule) = if hidden {
            (FlushHandle::HiddenTimeout, FlushSchedule::Timeout(HIDDEN_FLUSH_DELAY_MS))
        } else if wait > 0.0 {
            (FlushHandle::IntervalTimeout, FlushSchedule::Timeout(wait))
        } else {
            (FlushHandle::Frame, FlushSchedule::Frame)
        };
        runtime.flush_handle = Some(handle);
        Enqueued { schedule: Some(schedule), ..Enqueued::default() }
    }

    /// A [`FlushSchedule::Timeout`] fired: the output was flushed (`Ok`), or it is shown on the next
    /// frame (`Err` with the schedule to follow).
    pub fn flush_timer_fired(&mut self, session_id: &str, now_ms: f64, hidden: bool) -> Result<Flushed, FlushSchedule> {
        let waits_for_frame = self
            .runtimes
            .get(session_id)
            .is_some_and(|runtime| runtime.flush_handle == Some(FlushHandle::IntervalTimeout) && !hidden);
        if !waits_for_frame {
            return Ok(self.flush(session_id, now_ms));
        }
        if let Some(runtime) = self.runtimes.get_mut(session_id) {
            runtime.flush_handle = Some(FlushHandle::Frame);
        }
        Err(FlushSchedule::Frame)
    }

    /// Shows the queued output of a session. Cancels its pending flush: a timer or frame that still
    /// fires afterwards finds nothing to do.
    pub fn flush(&mut self, session_id: &str, now_ms: f64) -> Flushed {
        let Some(runtime) = self.runtimes.get_mut(session_id) else { return Flushed::default() };
        runtime.flush_handle = None;
        if runtime.queue.is_empty() {
            return Flushed::default();
        }
        runtime.flushed_at = now_ms;
        for (kind, text) in std::mem::take(&mut runtime.queue) {
            if !text.is_empty() {
                runtime.console.append(kind, &text);
            }
        }
        let lines = self.completed_lines(session_id);
        let open_browser = self.lines_seen(session_id, &lines);
        self.bump(session_id);
        Flushed { changed: true, lines, open_browser }
    }

    /// The process output lines completed since the last call (the IDE's own messages are not output).
    fn completed_lines(&mut self, session_id: &str) -> Vec<CompletedLine> {
        let Some(runtime) = self.runtimes.get_mut(session_id) else { return Vec::new() };
        runtime.console.take_completed().into_iter().filter(|line| line.kind != ChunkKind::System).collect()
    }

    /// Reads server start-up lines: the HTTP port of the session, and whether to open the browser.
    fn lines_seen(&mut self, session_id: &str, lines: &[CompletedLine]) -> bool {
        let Some(config) = self.session(session_id).map(|session| session.config.clone()) else { return false };
        let mut open_browser = false;
        for line in lines {
            if config.kind == "tomcat" && line.text.contains("Server startup in") {
                let port = param(&config, "httpPort").filter(|port| !port.is_empty()).unwrap_or("8080");
                let port = port.parse::<u16>().ok();
                let wants_browser = param(&config, "openBrowser") == Some("true");
                if let Some(session) = self.session_mut(session_id) {
                    session.http_port = port;
                }
                if let Some(runtime) = self.runtimes.get_mut(session_id).filter(|runtime| !runtime.browser_opened)
                    && wants_browser
                {
                    runtime.browser_opened = true;
                    open_browser = true;
                }
            }
            if let Some(port) = started_port(&line.text)
                && let Some(session) = self.session_mut(session_id)
            {
                session.http_port = Some(port);
            }
        }
        open_browser
    }

    /// The window was hidden or shown. Frames stop firing while it is hidden, so a pending frame
    /// flush moves to a timeout (and back on return): cancel each returned session's pending flush and
    /// follow its new schedule.
    pub fn reschedule_console_flushes(&mut self, hidden: bool) -> Vec<(String, FlushSchedule)> {
        let mut schedules = Vec::new();
        for (id, runtime) in &mut self.runtimes {
            let Some(handle) = runtime.flush_handle else { continue };
            if hidden == (handle != FlushHandle::Frame) {
                continue;
            }
            let (handle, schedule) = if hidden {
                (FlushHandle::HiddenTimeout, FlushSchedule::Timeout(HIDDEN_FLUSH_DELAY_MS))
            } else {
                (FlushHandle::Frame, FlushSchedule::Frame)
            };
            runtime.flush_handle = Some(handle);
            schedules.push((id.clone(), schedule));
        }
        schedules.sort_by(|a, b| a.0.cmp(&b.0));
        schedules
    }

    /// Applies a changed cycle buffer size to every open console.
    pub fn apply_console_settings(&mut self, char_limit: Option<usize>) {
        self.console_char_limit = char_limit;
        let ids: Vec<String> = self.runtimes.keys().cloned().collect();
        for id in ids {
            if let Some(runtime) = self.runtimes.get_mut(&id) {
                runtime.console.set_max_chars(char_limit);
            }
            self.bump(&id);
        }
    }

    /// Opens the tab of a start: the given session's (restarted in place), else a new one.
    fn open_session(
        &mut self,
        config: &RunConfig,
        reuse: Option<&str>,
        base_config: Option<RunConfig>,
        now_ms: f64,
    ) -> (String, u64) {
        let (id, generation) = match reuse.filter(|id| self.session(id).is_some()) {
            Some(id) => (id.to_string(), self.generation(id).unwrap_or(0) + 1),
            None => {
                self.session_seq += 1;
                (format!("run{}", self.session_seq), 0)
            }
        };
        self.runtimes.insert(id.clone(), SessionRuntime::new(self.console_char_limit, generation));
        let session = RunSession {
            id: id.clone(),
            config: config.clone(),
            run_id: None,
            status: SessionStatus::Starting,
            exit_code: None,
            started_at: now_ms,
            ended_at: None,
            stdin_closed: false,
            stopped: false,
            base_config,
            terminal: false,
            http_port: None,
        };
        match self.session_mut(&id) {
            Some(existing) => *existing = session,
            None => self.sessions.push(session),
        }
        self.active_session = Some(id.clone());
        self.bump(&id);
        (id, generation)
    }

    /// Handles the backend's exit event of a run. Returns the session to finish: wait
    /// [`FINISH_DELAY_MS`] (output still on its way lands first), then call [`Self::finish_session`].
    pub fn on_run_exited(&mut self, run_id: RunId, code: Option<i32>) -> Option<String> {
        if let Some(session) = self.sessions.iter().find(|session| session.run_id == Some(run_id)) {
            return Some(session.id.clone());
        }
        self.pending_exits.push((run_id, code));
        if self.pending_exits.len() > MAX_PENDING_EXITS {
            self.pending_exits.remove(0);
        }
        None
    }

    /// Applies the exit of a session's process. `generation`: the start that exited (see
    /// [`Self::generation`]); a tab restarted since is left alone.
    pub fn finish_session(
        &mut self,
        session_id: &str,
        generation: Option<u64>,
        code: Option<i32>,
        now_ms: f64,
        context: FinishContext,
    ) -> Finished {
        if self.session(session_id).is_none() {
            return Finished::default();
        }
        if self.generation(session_id).is_none() {
            // Nothing to flush: the run's output is gone already. The exit is still the authority on
            // the process being over, so the tab must not stay "stopping" for ever.
            self.end_session(session_id, SessionStatus::Finished, code, now_ms);
            return Finished::default();
        }
        if generation.is_some() && self.generation(session_id) != generation {
            return Finished::default();
        }
        let mut lines = self.flush(session_id, now_ms).lines;
        if let Some(runtime) = self.runtimes.get_mut(session_id) {
            runtime.console.flush();
        }
        lines.extend(self.completed_lines(session_id));
        self.end_session(session_id, SessionStatus::Finished, code, now_ms);
        self.bump(session_id);
        let stopped = self.session(session_id).is_some_and(|session| session.stopped);
        let notification = self.exit_notification(session_id, code, context);
        Finished { lines, notification, exited: Some((code, stopped)) }
    }

    fn end_session(&mut self, session_id: &str, status: SessionStatus, code: Option<i32>, now_ms: f64) {
        if let Some(session) = self.session_mut(session_id) {
            session.status = status;
            session.exit_code = code;
            session.ended_at = Some(now_ms);
        }
    }

    /// The notification of a run that failed while the Run tool window was not showing.
    fn exit_notification(
        &self,
        session_id: &str,
        code: Option<i32>,
        context: FinishContext,
    ) -> Option<NotificationRequest> {
        static NO_GO_VERSION: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)No version (?:is )?set for (?:shim: )?go").expect("the Go version pattern is valid")
        });
        let session = self.session(session_id).filter(|session| !session.stopped)?;
        let code = code.filter(|code| *code != 0)?;
        let action = |label: &str, command: String| NotificationAction { label: label.to_string(), command };
        let mut request = NotificationRequest::new(NotificationLevel::Error, "");
        request.group = Some("run".to_string());
        request.key = Some(format!("run-exit-{session_id}"));
        let output = self.console(session_id).map(ConsoleModel::text).unwrap_or_default();
        if session.config.kind == "go" && !context.go_root_set && NO_GO_VERSION.is_match(&output) {
            request.title = "Choose a Go SDK".to_string();
            request.body = Some(
                "The Go version manager has no SDK selected. Choose an installed SDK in Go settings and run again."
                    .to_string(),
            );
            request.actions = vec![action("Go Settings…", "file.settings:languages.go".to_string())];
            return Some(request);
        }
        if context.run_window_visible {
            return None;
        }
        if context.failed_tests > 0 {
            request.title = format!("Tests failed: {}", context.failed_tests);
            request.body = Some(session.config.name.clone());
        } else {
            request.title = format!("'{}' exited with code {code}", session.config.name);
        }
        request.actions = vec![
            action("Show", format!("run.showSession:{session_id}")),
            action("Rerun", format!("run.rerunSession:{session_id}")),
        ];
        Some(request)
    }

    /// The request preparing a session's launch (cancelled when the session is stopped meanwhile).
    pub fn set_resolve_request(&mut self, session_id: &str, request: Option<RequestId>) {
        if let Some(runtime) = self.runtimes.get_mut(session_id) {
            runtime.resolve_request = request;
        }
    }

    fn resolve_aborted(&self, session_id: &str) -> bool {
        self.runtimes.get(session_id).is_some_and(|runtime| runtime.resolve_aborted)
    }

    /// Stops a session. Stopping keeps the process live (IDEA: the Stop button stays until the process
    /// is gone); a second Stop meanwhile skips the grace period and kills it.
    pub fn stop_session(&mut self, session_id: &str, now_ms: f64) -> Option<StopAction> {
        let session = self.session(session_id)?;
        let (status, run_id) = (session.status, session.run_id);
        let request = self.runtimes.get(session_id).and_then(|runtime| runtime.resolve_request);
        if let Some(request) = request.filter(|_| status == SessionStatus::Starting) {
            if let Some(runtime) = self.runtimes.get_mut(session_id) {
                runtime.resolve_aborted = true;
            }
            let session = self.session_mut(session_id)?;
            session.stopped = true;
            session.status = SessionStatus::Finished;
            session.ended_at = Some(now_ms);
            return Some(StopAction::CancelRequest(request));
        }
        let run_id = run_id.filter(|_| status.is_live())?;
        let session = self.session_mut(session_id)?;
        session.stopped = true;
        session.status = SessionStatus::Stopping;
        Some(StopAction::StopRun(run_id))
    }

    /// The backend could not stop the process: it still runs.
    pub fn stop_failed(&mut self, session_id: &str, message: &str) -> NotificationRequest {
        if let Some(session) = self.session_mut(session_id) {
            session.status = SessionStatus::Running;
        }
        let mut request = NotificationRequest::new(NotificationLevel::Error, "Cannot stop process");
        request.group = Some("run".to_string());
        request.body = Some(message.to_string());
        request
    }

    /// Replaces the pending (typed, unsent) console input.
    pub fn set_pending_input(&mut self, session_id: &str, text: &str) {
        let Some(runtime) = self.runtimes.get_mut(session_id).filter(|runtime| runtime.input != text) else { return };
        runtime.input = text.to_string();
        self.bump(session_id);
    }

    fn running_run(&self, session_id: &str) -> Option<RunId> {
        self.session(session_id).filter(|session| session.status == SessionStatus::Running)?.run_id
    }

    /// Sends a line of console input, echoed the way IDEA does (user input coloured in the console).
    pub fn send_stdin(
        &mut self,
        session_id: &str,
        text: &str,
        now_ms: f64,
        hidden: bool,
    ) -> Option<(StdinWrite, Enqueued)> {
        let run = self.running_run(session_id)?;
        let echo = format!("\x1b[32m{text}\x1b[39m\n");
        let enqueued = self.enqueue(session_id, ChunkKind::Stdout, &echo, now_ms, hidden);
        Some((StdinWrite { run, data: format!("{text}\n") }, enqueued))
    }

    /// Keystrokes typed into a console that emulates a terminal, sent as they are.
    pub fn write_terminal(&self, session_id: &str, data: &str) -> Option<StdinWrite> {
        Some(StdinWrite { run: self.running_run(session_id)?, data: data.to_string() })
    }

    /// Writing to the process failed: says so in the console.
    pub fn stdin_failed(
        &mut self,
        session_id: &str,
        message: &str,
        now_ms: f64,
        hidden: bool,
        i18n: &mut I18n,
    ) -> Enqueued {
        let text = i18n.tx_built(&["Cannot write to stdin: ", "\n"], &[Param::from(message)]);
        self.enqueue(session_id, ChunkKind::System, &text, now_ms, hidden)
    }

    /// The terminal console's size in cells; the process's terminal follows it (the returned run is
    /// resized now, a process still starting once it has an id).
    pub fn resize_terminal(&mut self, session_id: &str, cols: u16, rows: u16) -> Option<RunId> {
        if cols < 1 || rows < 1 {
            return None;
        }
        self.runtimes.get_mut(session_id)?.terminal_size = Some((cols, rows));
        self.running_run(session_id)
    }

    /// The run whose stdin "Send EOF" closes; then call [`Self::stdin_closed`].
    pub fn close_stdin(&self, session_id: &str) -> Option<RunId> {
        self.running_run(session_id)
    }

    pub fn stdin_closed(&mut self, session_id: &str) {
        if let Some(session) = self.session_mut(session_id) {
            session.stdin_closed = true;
        }
    }

    pub fn clear_console(&mut self, session_id: &str) {
        if let Some(runtime) = self.runtimes.get_mut(session_id) {
            runtime.console.clear();
            if let Some(terminal) = &mut runtime.terminal {
                terminal.clear();
            }
        }
        self.bump(session_id);
    }

    /// Closing a Run tab: a process still running is terminated once the user agrees, as IDEA asks.
    pub fn close_session_tab(&mut self, session_id: &str, settings: &RunSettings, now_ms: f64) -> CloseTab {
        let Some(session) = self.session(session_id) else { return CloseTab::Missing };
        if session.is_live() && settings.confirm_terminate {
            return CloseTab::Confirm { name: session.config.name.clone() };
        }
        CloseTab::Closed(self.close_session(session_id, now_ms))
    }

    /// Removes a session's tab, stopping its process. The tab next to it becomes the active one.
    pub fn close_session(&mut self, session_id: &str, now_ms: f64) -> Option<StopAction> {
        let index = self.sessions.iter().position(|session| session.id == session_id)?;
        let stop = if self.sessions[index].is_live() { self.stop_session(session_id, now_ms) } else { None };
        self.runtimes.remove(session_id);
        self.sessions.remove(index);
        if self.active_session.as_deref() == Some(session_id) {
            let next = self.sessions.get(index).or(self.sessions.last());
            self.active_session = next.map(|session| session.id.clone());
        }
        self.versions.remove(session_id);
        stop
    }

    /// Stops everything and forgets sessions and configurations (project closed).
    pub fn reset_runs(&mut self, now_ms: f64) -> Vec<StopAction> {
        let live: Vec<String> = self.running_sessions().iter().map(|session| session.id.clone()).collect();
        let stops = live.iter().filter_map(|id| self.stop_session(id, now_ms)).collect();
        // The session counter and the ids ever listed outlive a project, as the window does.
        *self = Self {
            project_root: self.project_root.take(),
            console_char_limit: self.console_char_limit,
            session_seq: self.session_seq,
            persisting: self.persisting,
            listed: std::mem::take(&mut self.listed),
            ..Self::default()
        };
        stops
    }

    /// Development and the performance harness only: a running session without a process, for
    /// generated output ([`sample_line`]).
    pub fn start_sample_session(&mut self, now_ms: f64) -> String {
        let config = blank_config(format!("sample-output-{}", self.session_seq + 1), "Sample output", "shell");
        let (id, _) = self.open_session(&config, None, None, now_ms);
        if let Some(session) = self.session_mut(&id) {
            session.status = SessionStatus::Running;
            session.stdin_closed = true;
        }
        id
    }
}

/// Line `i` of the generated sample output, as from a chatty process: log lines, stack frames among
/// them; every 101st goes to stderr.
pub fn sample_line(i: usize) -> (ChunkKind, String) {
    let text = if i % 20 == 19 {
        format!("\tat com.acme.orders.service.OrderService.process(OrderService.kt:{})", i % 300 + 1)
    } else if i % 7 == 6 {
        format!(
            "2026-11-05 10:42:01.{:03} DEBUG 4242 --- [           main] o.s.web.servlet.DispatcherServlet        : \
             GET \"/api/orders/{i}\", parameters={{}}",
            i % 1000
        )
    } else {
        format!(
            "2026-11-05 10:42:{:02}.{:03}  INFO 4242 --- [nio-8080-exec-{}] c.a.orders.service.OrderService  : \
             processed order #{i} in {} ms",
            i % 60,
            i % 1000,
            i % 10,
            i % 97
        )
    };
    (if i % 101 == 100 { ChunkKind::Stderr } else { ChunkKind::Stdout }, format!("{text}\n"))
}

// ---------------------------------------------------------------------------------------------------
// Saving

/// What saving user configurations needs next.
#[derive(Clone, Debug, PartialEq)]
pub enum SaveStep {
    /// A step of finding the module of an application saved without one; answer through the
    /// [`SaveConfigs`] method the [`ModuleStep`] names.
    Module(ModuleStep),
    /// Save `configs` (and suppress the removed detections), then reload the configurations.
    Save { configs: Vec<RunConfig>, removed_detected_ids: Vec<String> },
}

/// Persists user configurations: an Application or Spring Boot configuration saved without a module
/// gets the module of its main class first.
#[derive(Clone, Debug)]
pub struct SaveConfigs {
    user: Vec<RunConfig>,
    removed_detected_ids: Vec<String>,
    next: usize,
    finder: Option<ModuleFinder>,
}

impl SaveConfigs {
    pub fn start(user: Vec<RunConfig>, removed_detected_ids: Vec<String>) -> (Self, SaveStep) {
        let mut save = Self { user, removed_detected_ids, next: 0, finder: None };
        let step = save.advance();
        (save, step)
    }

    fn advance(&mut self) -> SaveStep {
        while let Some(config) = self.user.get_mut(self.next) {
            if is_jvm_application(&config.kind) && config.extras.module.is_none() {
                match find_java_module(config) {
                    ModuleLookup::Known(module) => config.extras.module = module,
                    ModuleLookup::Search(finder) => {
                        let step = finder.first_step();
                        self.finder = Some(finder);
                        return SaveStep::Module(step);
                    }
                }
            }
            self.next += 1;
        }
        let configs = self.user.iter().map(|config| RunConfig { detected: false, ..config.clone() }).collect();
        SaveStep::Save { configs, removed_detected_ids: self.removed_detected_ids.clone() }
    }

    /// Continues with a step of the module search; its end settles the configuration's module.
    fn module_step(&mut self, step: Option<ModuleStep>) -> SaveStep {
        let module = match step {
            Some(ModuleStep::Done(module)) => module,
            Some(step) => return SaveStep::Module(step),
            None => None,
        };
        if let Some(config) = self.user.get_mut(self.next) {
            config.extras.module = module;
        }
        self.finder = None;
        self.next += 1;
        self.advance()
    }

    pub fn files_found(&mut self, found: Vec<FoundSource>) -> SaveStep {
        let step = self.finder.as_mut().map(|finder| finder.files_found(found));
        self.module_step(step)
    }

    pub fn configs_listed(&mut self, configs: Vec<RunConfig>) -> SaveStep {
        let step = self.finder.as_mut().map(|finder| finder.configs_listed(configs));
        self.module_step(step)
    }

    pub fn sources_read(&mut self, texts: &[String], modules: &[&str]) -> SaveStep {
        let step = self.finder.as_ref().map(|finder| finder.sources_read(texts, modules));
        self.module_step(step)
    }
}

// ---------------------------------------------------------------------------------------------------
// Starting

/// How a configuration is started.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StartOptions {
    /// Restart this session's tab (rerun) instead of looking for one.
    pub reuse: Option<String>,
    /// The unfiltered configuration of a derived run (Rerun Failed Tests).
    pub base_config: Option<RunConfig>,
    /// Whether to show the Run tool window; `None`: as the configuration and the settings say.
    pub show: Option<bool>,
    /// Launch this instead of the configuration (e.g. with a debug agent); the tab still shows the
    /// configuration. Skips before launch tasks.
    pub launch: Option<RunConfig>,
    /// Before launch tasks already ran (or must not run).
    pub prepared: bool,
    /// Configurations whose launch led to this one (before launch tasks, compounds): breaks cycles.
    pub chain: Vec<String>,
}

/// What a start reads from outside the store.
#[derive(Clone, Copy, Debug)]
pub struct StartEnv<'a> {
    pub types: &'a RunConfigTypes,
    pub settings: &'a RunSettings,
}

/// Something a start asks for on the side; nothing waits for it.
#[derive(Clone, Debug, PartialEq)]
pub enum StartEffect {
    Notify(NotificationRequest),
    /// Show the Run tool window (`focus`: and give it the focus).
    ShowRunWindow {
        focus: bool,
    },
    /// The configuration ran: the Services tool window lists it again after a restart.
    RecordRun(String),
    /// Give the process's terminal the size the console reported.
    Resize {
        run: RunId,
        cols: u16,
        rows: u16,
    },
    /// The process exited before its id was known: finish the session as after
    /// [`RunStore::on_run_exited`].
    Exited {
        session_id: String,
        code: Option<i32>,
    },
}

/// What starting a configuration needs next. A step with a continuation names the [`StartRun`]
/// method that takes its answer; the others end the start.
#[derive(Clone, Debug)]
pub enum StartStep {
    /// A CMake configure or build configuration: dispatch the build. Nothing runs in a Run tab.
    Build(BuildRequest),
    /// Run the before launch tasks of `config`, then [`StartRun::before_launch_done`].
    BeforeLaunch { config: Box<RunConfig>, chain: Vec<String>, show: Option<bool> },
    /// A compound: start each configuration with this chain. Nothing else starts.
    StartChildren { configs: Vec<RunConfig>, chain: Vec<String>, show: Option<bool> },
    /// Ask whether to stop the `count` running instances of `name`, then [`StartRun::rerun_confirmed`].
    ConfirmRerun { name: String, count: usize },
    /// Stop these sessions and wait until their processes are gone (no time limit: a first Stop waits
    /// for the shutdown, a second one kills it), then [`StartRun::instances_stopped`].
    StopAndWait { session_ids: Vec<String> },
    /// Save the open documents (best effort), then [`StartRun::saved`].
    SaveAll,
    /// "Build and run using: Gradle": the module's Gradle build may compile and launch the
    /// application instead; answer with [`StartRun::delegation_resolved`].
    GradleDelegation(Box<RunConfig>),
    /// Drive [`StartRun::preparation`] from this step to its end, calling [`StartRun::report_progress`]
    /// along the way and registering the cancellable request with [`RunStore::set_resolve_request`];
    /// then [`StartRun::java_prepared`].
    PrepareJava(PrepareStep),
    /// The configuration's type adjusts the launch through the backend; answer with
    /// [`StartRun::type_prepared`].
    PrepareType(Box<RunConfig>),
    /// Make the launch what the process host gets: test reporting for a test run, the Gradle boot
    /// script otherwise; answer with [`StartRun::launch_finalized`].
    FinalizeLaunch(Box<RunConfig>),
    /// Start the process (retrying what a run target needs from the user), streaming its output to
    /// [`RunStore::output`] with [`StartRun::generation`]; answer with [`StartRun::process_started`].
    RunStart(Box<RunConfig>),
    /// The start ended: the session it started, if any (which may have failed).
    Done(Option<String>),
}

/// Starts a configuration, reusing the tab of a finished (or the given) session for the same
/// configuration. As in IDEA, a configuration that doesn't allow parallel runs asks before stopping
/// its running instances; rerunning a tab restarts it without asking.
#[derive(Clone, Debug)]
pub struct StartRun {
    config: RunConfig,
    options: StartOptions,
    /// The tab to restart, once chosen.
    session: Option<String>,
    /// Running instances to stop first.
    others: Vec<String>,
    /// The finished tab of the same configuration when the start began.
    idle: Option<String>,
    session_id: Option<String>,
    generation: u64,
    launch: Option<RunConfig>,
    preparation: Option<JavaPreparation>,
    effects: Vec<StartEffect>,
}

impl StartRun {
    pub fn start(config: &RunConfig, options: StartOptions, store: &mut RunStore, env: StartEnv) -> (Self, StartStep) {
        let config = with_run_target(config, env.settings);
        let mut start = Self {
            config,
            options,
            session: None,
            others: Vec::new(),
            idle: None,
            session_id: None,
            generation: 0,
            launch: None,
            preparation: None,
            effects: Vec::new(),
        };
        let step = start.begin(store, env);
        (start, step)
    }

    /// The configuration being started (on its run target).
    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    /// The session of this start, once its tab is open.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Which start of the tab this is: output is accepted for this generation only.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn take_effects(&mut self) -> Vec<StartEffect> {
        std::mem::take(&mut self.effects)
    }

    /// The launch preparation of an Application or Spring Boot configuration, while it runs.
    pub fn preparation(&mut self) -> Option<&mut JavaPreparation> {
        self.preparation.as_mut()
    }

    fn before_launch(&self) -> Option<StartStep> {
        if self.config.extras.before_launch.is_empty() {
            return None;
        }
        let chain = self.options.chain.iter().cloned().chain([self.config.id.clone()]).collect();
        Some(StartStep::BeforeLaunch { config: Box::new(self.config.clone()), chain, show: self.options.show })
    }

    fn begin(&mut self, store: &mut RunStore, env: StartEnv) -> StartStep {
        let launches_itself = self.options.launch.is_none();
        if launches_itself && is_cmake_build(&self.config) {
            return StartStep::Build(build_cmake(&self.config.id, self.options.show != Some(false)));
        }
        if self.config.kind == "compound" {
            return self.before_launch().unwrap_or_else(|| self.children(store));
        }
        // IDEA offers only Debug for Remote JVM Debug and the like: there is nothing to run.
        if launches_itself && env.types.get(&self.config.kind).is_some_and(|ty| ty.debug_only) {
            let title = format!("'{}' can only be debugged", self.config.name);
            let mut request = NotificationRequest::new(NotificationLevel::Info, title);
            request.group = Some("run".to_string());
            request.key = Some("run.debugOnly".to_string());
            request.body = Some("It attaches the debugger to a process started elsewhere.".to_string());
            let command = format!("run.debugConfig:{}", self.config.id);
            request.actions = vec![NotificationAction { label: "Debug".to_string(), command }];
            self.effects.push(StartEffect::Notify(request));
            return StartStep::Done(None);
        }
        if launches_itself
            && !self.options.prepared
            && let Some(step) = self.before_launch()
        {
            return step;
        }
        self.instances(store, env)
    }

    fn children(&self, store: &RunStore) -> StartStep {
        let chain = self.options.chain.iter().cloned().chain([self.config.id.clone()]).collect();
        let configs = compound_children(&self.config, store.configs());
        StartStep::StartChildren { configs, chain, show: self.options.show }
    }

    /// The before launch tasks ran (`ok`), or one failed and the launch is off.
    pub fn before_launch_done(&mut self, ok: bool, store: &mut RunStore, env: StartEnv) -> StartStep {
        if !ok {
            return StartStep::Done(None);
        }
        if self.config.kind == "compound" {
            return self.children(store);
        }
        self.instances(store, env)
    }

    /// Looks at the sessions as they are now: the tab to use and the instances in its way.
    fn instances(&mut self, store: &mut RunStore, env: StartEnv) -> StartStep {
        let reuse = self.options.reuse.as_deref();
        self.session = reuse.filter(|id| store.session(id).is_some()).map(str::to_string);
        let same = |session: &&RunSession| session.config.id == self.config.id;
        self.others = if self.config.allow_parallel {
            Vec::new()
        } else {
            let chosen = self.session.as_deref();
            let other = |session: &&RunSession| session.is_live() && Some(session.id.as_str()) != chosen;
            store.sessions.iter().filter(same).filter(other).map(|session| session.id.clone()).collect()
        };
        let idle = store.sessions.iter().filter(same).find(|session| !session.is_live());
        self.idle = idle.map(|session| session.id.clone());
        if !self.others.is_empty() && env.settings.confirm_rerun {
            let own = self.session.as_deref().and_then(|id| store.session(id)).is_some_and(RunSession::is_live);
            let count = self.others.len() + usize::from(own);
            return StartStep::ConfirmRerun { name: self.config.name.clone(), count };
        }
        self.stop_instances(store)
    }

    pub fn rerun_confirmed(&mut self, confirmed: bool, store: &mut RunStore) -> StartStep {
        if confirmed { self.stop_instances(store) } else { StartStep::Done(None) }
    }

    fn stop_instances(&mut self, store: &RunStore) -> StartStep {
        if self.session.is_none() {
            self.session = self.others.first().cloned();
        }
        let mut stopping = self.others.clone();
        if let Some(session) = self.session.as_deref().and_then(|id| store.session(id))
            && session.is_live()
            && !stopping.contains(&session.id)
        {
            stopping.push(session.id.clone());
        }
        if stopping.is_empty() { StartStep::SaveAll } else { StartStep::StopAndWait { session_ids: stopping } }
    }

    /// The old processes are gone (they may have held a port or a lock).
    pub fn instances_stopped(&mut self) -> StartStep {
        StartStep::SaveAll
    }

    /// The documents were saved (or could not be: saving is best effort). Opens the tab.
    pub fn saved(&mut self, store: &mut RunStore, env: StartEnv, now_ms: f64) -> StartStep {
        let reuse = self.session.take().or_else(|| self.idle.take());
        let (session_id, generation) =
            store.open_session(&self.config, reuse.as_deref(), self.options.base_config.clone(), now_ms);
        self.session_id = Some(session_id);
        self.generation = generation;
        self.effects.push(StartEffect::RecordRun(self.config.id.clone()));
        let extras = &self.config.extras;
        if self.options.show != Some(false) && extras.focus_tool_window {
            self.effects.push(StartEffect::ShowRunWindow { focus: true });
        } else if self.options.show.or(extras.open_tool_window).unwrap_or(env.settings.show_on_start) {
            self.effects.push(StartEffect::ShowRunWindow { focus: false });
        }
        // A Maven/Gradle configuration's own command is its build; Rebuild adds clean.
        let launch = match &self.options.launch {
            Some(launch) => launch.clone(),
            None => apply_build_policy_to_command(&self.config),
        };
        let jvm = is_jvm_application(&launch.kind);
        self.launch = Some(launch.clone());
        if jvm { StartStep::GradleDelegation(Box::new(launch)) } else { self.prepare_type(store, env) }
    }

    /// Whether the tab was restarted (or closed) since this start opened it.
    fn stale(&self, store: &RunStore) -> bool {
        self.session_id.as_deref().and_then(|id| store.generation(id)) != Some(self.generation)
    }

    fn done(&self) -> StartStep {
        StartStep::Done(self.session_id.clone())
    }

    fn say(&self, store: &mut RunStore, kind: ChunkKind, text: &str, now_ms: f64) {
        if let Some(id) = &self.session_id {
            store.enqueue(id, kind, text, now_ms, false);
            store.flush(id, now_ms);
        }
    }

    /// The Gradle build takes over the launch (`Some`), or the application is resolved here (`None`).
    pub fn delegation_resolved(
        &mut self,
        delegated: Result<Option<RunConfig>, String>,
        store: &mut RunStore,
        env: StartEnv,
        now_ms: f64,
        i18n: &mut I18n,
    ) -> StartStep {
        if self.stale(store) {
            return self.done();
        }
        match delegated {
            Err(message) => self.start_failed(&message, store, now_ms, i18n),
            Ok(Some(delegated)) => {
                self.launch = Some(delegated);
                self.prepare_type(store, env)
            }
            Ok(None) => self.resolve_application(store, env, now_ms, i18n),
        }
    }

    /// Builds the module and resolves `${classpath}` and the project JDK of a Java application
    /// configuration. Always prepared, exactly like Debug: even an explicit classpath needs the
    /// module, the project JDK and the selected build, and both modes must share the same result.
    fn resolve_application(&mut self, store: &mut RunStore, env: StartEnv, now_ms: f64, i18n: &mut I18n) -> StartStep {
        let Some(launch) = self.launch.clone() else { return self.done() };
        if parse_java_args(&launch.args).main_class.is_none() {
            let text = format!(
                "No main class in '{}': expected java [options] -cp ${{classpath}} <main class> [args]\n",
                launch.name
            );
            self.say(store, ChunkKind::Stderr, &text, now_ms);
            return self.launch_ended(store, now_ms);
        }
        let (preparation, step) = JavaPreparation::start(launch, None, true, i18n);
        self.preparation = Some(preparation);
        self.report_progress(store, now_ms);
        match step {
            PrepareStep::Done(prepared) => self.java_prepared(Ok(*prepared), store, env, now_ms),
            PrepareStep::Failed(error) => self.java_prepared(Err(error), store, env, now_ms),
            step => StartStep::PrepareJava(step),
        }
    }

    /// Shows what the preparation reported since the last call in the session's console.
    pub fn report_progress(&mut self, store: &mut RunStore, now_ms: f64) {
        let reports = self.preparation.as_mut().map(JavaPreparation::take_reports).unwrap_or_default();
        for report in reports {
            let kind = if report.stderr { ChunkKind::Stderr } else { ChunkKind::System };
            self.say(store, kind, &report.text, now_ms);
        }
    }

    /// The preparation ended. A caller that offers to download a missing JDK reads the launch
    /// information's hints, or the error's message, itself.
    pub fn java_prepared(
        &mut self,
        outcome: Result<PreparedLaunch, JavaLaunchError>,
        store: &mut RunStore,
        env: StartEnv,
        now_ms: f64,
    ) -> StartStep {
        self.report_progress(store, now_ms);
        self.preparation = None;
        if let Some(id) = &self.session_id {
            store.set_resolve_request(id, None);
        }
        if self.stale(store) {
            return self.done();
        }
        // Stop was pressed while the launch was being prepared: an answer that still arrives,
        // a successful one too, starts nothing.
        let aborted = self.session_id.as_deref().is_some_and(|id| store.resolve_aborted(id));
        let error = match outcome {
            Ok(prepared) if !aborted => {
                self.launch = Some(prepared.resolved);
                return self.prepare_type(store, env);
            }
            Ok(_) => None,
            Err(error) => Some(error),
        };
        let Some(error) = error.filter(|error| !aborted && !error.is_cancelled()) else {
            if let Some(id) = &self.session_id
                && let Some(session) = store.session_mut(id)
            {
                session.stopped = true;
            }
            return self.launch_ended(store, now_ms);
        };
        let message = error.message().to_string();
        let text = match error {
            JavaLaunchError::BuildFailed(_) => format!("{message}\n"),
            _ => format!("Cannot resolve the classpath: {message}\n"),
        };
        self.say(store, ChunkKind::Stderr, &text, now_ms);
        let mut request =
            NotificationRequest::new(NotificationLevel::Error, format!("Cannot run '{}'", self.config.name));
        request.group = Some("run".to_string());
        request.body = Some(message);
        self.effects.push(StartEffect::Notify(request));
        self.launch_ended(store, now_ms)
    }

    /// The launch could not be resolved: the tab ends, as stopped when the user stopped it.
    fn launch_ended(&mut self, store: &mut RunStore, now_ms: f64) -> StartStep {
        if let Some(id) = &self.session_id
            && let Some(session) = store.session_mut(id)
        {
            session.status = if session.stopped { SessionStatus::Finished } else { SessionStatus::Failed };
            session.ended_at = Some(now_ms);
        }
        self.done()
    }

    fn prepare_type(&mut self, store: &mut RunStore, env: StartEnv) -> StartStep {
        let Some(launch) = self.launch.clone() else { return self.done() };
        if env.types.get(&launch.kind).is_some_and(|ty| ty.prepares_launch) {
            return StartStep::PrepareType(Box::new(launch));
        }
        self.finalize(store)
    }

    /// The type adjusted the launch, or could not.
    pub fn type_prepared(
        &mut self,
        prepared: Result<RunConfig, String>,
        store: &mut RunStore,
        now_ms: f64,
        i18n: &mut I18n,
    ) -> StartStep {
        let launch = match prepared {
            Ok(launch) => launch,
            Err(message) => return self.start_failed(&message, store, now_ms, i18n),
        };
        if launch.kind == "tomcat" {
            store.active_session = self.session_id.clone();
        }
        self.launch = Some(launch);
        if self.stale(store) {
            return self.done();
        }
        self.finalize(store)
    }

    fn finalize(&mut self, store: &mut RunStore) -> StartStep {
        let (Some(mut launch), Some(id)) = (self.launch.clone(), self.session_id.clone()) else { return self.done() };
        // Debug launches read the agent's output lines: never through a terminal.
        if self.options.launch.is_some() || launch.is_test {
            launch.extras.emulate_terminal = false;
        }
        let terminal = launch.extras.emulate_terminal;
        if let Some(runtime) = store.runtimes.get_mut(&id).filter(|_| terminal) {
            runtime.terminal = Some(TerminalOutput::default());
        }
        if let Some(session) = store.session_mut(&id) {
            session.terminal = terminal;
        }
        self.launch = Some(launch.clone());
        if param(&launch, "framework") == Some("plugin") {
            return StartStep::RunStart(Box::new(launch));
        }
        StartStep::FinalizeLaunch(Box::new(launch))
    }

    pub fn launch_finalized(
        &mut self,
        finalized: Result<RunConfig, String>,
        store: &mut RunStore,
        now_ms: f64,
        i18n: &mut I18n,
    ) -> StartStep {
        if self.stale(store) {
            return self.done();
        }
        match finalized {
            Ok(started) => StartStep::RunStart(Box::new(started)),
            Err(message) => self.start_failed(&message, store, now_ms, i18n),
        }
    }

    /// The process host answered.
    pub fn process_started(
        &mut self,
        started: Result<RunInfo, IdeError>,
        store: &mut RunStore,
        now_ms: f64,
        i18n: &mut I18n,
    ) -> StartStep {
        if self.stale(store) {
            return self.done();
        }
        let info = match started {
            Ok(info) => info,
            Err(error) => return self.start_failed(&error.message, store, now_ms, i18n),
        };
        let Some(id) = self.session_id.clone() else { return self.done() };
        if info.pty
            && let Some((cols, rows)) = store.runtimes.get(&id).and_then(|runtime| runtime.terminal_size)
        {
            self.effects.push(StartEffect::Resize { run: info.id, cols, rows });
        }
        // "Redirect input from": stdin is the file, the console takes no input.
        let stdin_closed = self.launch.as_ref().is_some_and(|launch| launch.extras.stdin_file.is_some());
        if let Some(session) = store.session_mut(&id) {
            session.run_id = Some(info.id);
            session.status = SessionStatus::Running;
            session.stdin_closed = stdin_closed;
        }
        // The process may already have exited (fast commands) before its id was known.
        if let Some(index) = store.pending_exits.iter().position(|(run, _)| *run == info.id) {
            let (_, code) = store.pending_exits.remove(index);
            self.effects.push(StartEffect::Exited { session_id: id, code });
        }
        self.done()
    }

    fn start_failed(&mut self, message: &str, store: &mut RunStore, now_ms: f64, i18n: &mut I18n) -> StartStep {
        let values = [Param::from(self.config.name.as_str()), Param::from(message)];
        let text = i18n.tx_built(&["Error running '", "': ", "\n"], &values);
        self.say(store, ChunkKind::Stderr, &text, now_ms);
        if let Some(id) = &self.session_id
            && let Some(session) = store.session_mut(id)
        {
            session.status = SessionStatus::Failed;
            session.ended_at = Some(now_ms);
        }
        let mut request =
            NotificationRequest::new(NotificationLevel::Error, format!("Error running '{}'", self.config.name));
        request.group = Some("run".to_string());
        request.body = Some(message.to_string());
        self.effects.push(StartEffect::Notify(request));
        self.done()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::application::{JvmContext, application_config};
    use crate::run::config_types::RunConfigType;
    use crate::run::configs::to_cwd;
    use ide_core::error::ErrorKind;
    use ide_lsp::jdtls::{JavaBuildStatus, JavaCompileError, JavaLaunchInfo};
    use ide_run::BuildPolicy;
    mod proofs8_removal;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn cfg(id: &str, kind: &str) -> RunConfig {
        RunConfig { command: "echo".into(), detected: true, ..blank_config(id, id, kind) }
    }

    fn app(class_name: &str, rel_path: &str) -> RunConfig {
        application_config(class_name, false, rel_path, JvmContext::default())
    }

    fn session(config: RunConfig, base_config: Option<RunConfig>) -> RunSession {
        RunSession {
            id: "s".into(),
            config,
            run_id: None,
            status: SessionStatus::Finished,
            exit_code: Some(0),
            started_at: 1.0,
            ended_at: Some(2.0),
            stdin_closed: true,
            stopped: false,
            base_config,
            terminal: false,
            http_port: None,
        }
    }

    /// A store of the project `/p` listing one Maven configuration.
    fn project_store() -> RunStore {
        let mut store = RunStore::new();
        store.project_root = Some("/p".into());
        store.set_configs(vec![cfg("maven:spring-boot:run", "maven")]);
        store
    }

    #[test]
    fn prefers_an_application_config_as_the_default_selection() {
        let named = RunConfig { name: "cargo run app".into(), ..cfg("cargo:run:app", "shell") };
        let list = [cfg("cargo:build", "shell"), RunConfig { is_test: true, ..cfg("cargo:test", "shell") }, named];
        assert_eq!(default_selection(&list).map(|config| config.id.as_str()), Some("cargo:run:app"));
        let spring = [cfg("maven:spring-boot:run", "maven"), cfg("application:demo.App", "spring-boot")];
        assert_eq!(default_selection(&spring).map(|config| config.id.as_str()), Some("application:demo.App"));
    }

    #[test]
    fn launches_the_configuration_as_saved_now_or_the_one_it_ran_with_once_deleted() {
        let ran = RunConfig { command: "python".into(), args: strings(&["init.py"]), ..cfg("app", "shell") };
        let edited = RunConfig { args: strings(&["main.py"]), ..ran.clone() };
        let ran_session = session(ran.clone(), None);
        let mut store = RunStore::new();
        store.set_configs(vec![edited.clone()]);
        assert_eq!(store.rerun_config(&ran_session), &edited);
        assert!(!store.config_removed("app"));
        store.set_configs(Vec::new());
        assert_eq!(store.rerun_config(&ran_session), &ran);
        assert!(store.config_removed("app"));
        // A run that was never a listed configuration isn't "removed".
        assert!(!store.config_removed("build:task"));
    }

    #[test]
    fn keeps_the_tests_of_a_derived_run() {
        let base = RunConfig { is_test: true, ..cfg("tests", "shell") };
        let derived = RunConfig { args: strings(&["-k", "failing"]), ..base.clone() };
        let mut store = RunStore::new();
        store.set_configs(vec![base.clone()]);
        assert_eq!(store.rerun_config(&session(derived.clone(), Some(base))), &derived);
    }

    fn report(
        class_name: &str,
        name: &str,
        failed: bool,
        message: Option<&str>,
        output: Option<&str>,
    ) -> TestCaseReport {
        TestCaseReport {
            class_name: class_name.into(),
            name: name.into(),
            status: if failed { TestCaseStatus::Failed } else { TestCaseStatus::Passed },
            duration_ms: (!failed).then_some(3.0),
            message: message.map(str::to_string),
            output: output.map(str::to_string),
        }
    }

    #[test]
    fn junit_reports_become_tree_results_with_failure_output_and_source_location() {
        let output = "AssertionError\n\tat org.junit.Assert.fail(Assert.java:1)\n\tat a.B$Inner.bad(B.java:42)";
        let events = test_report_events(&[
            report("a.B", "ok", false, None, None),
            report("a.B$Inner", "bad", true, Some("expected 1"), Some(output)),
        ]);
        let passed = ReportEvent::Result {
            path: strings(&["a.B", "ok"]),
            status: TestCaseStatus::Passed,
            duration_ms: Some(3.0),
        };
        assert_eq!(events[0], passed);
        let path = strings(&["a.B$Inner", "bad"]);
        assert!(events.contains(&ReportEvent::Output { path: path.clone(), line: "expected 1".into() }));
        let location = jvm_frame_target(Some("a.B$Inner"), "B.java", Some(42));
        assert_eq!(location.path, "a/B.java");
        assert!(events.contains(&ReportEvent::Location { path, location }));
    }

    #[test]
    fn junit_reports_locate_kotlin_tests_with_spaces_in_their_names_behind_a_class_loader_prefix() {
        let output = "AssertionFailedError\n\tat app//a.LibraryTest.cannot lend twice(LibraryTest.kt:43)";
        let events = test_report_events(&[report("a.LibraryTest", "cannot lend twice", true, None, Some(output))]);
        let location = jvm_frame_target(Some("a.LibraryTest"), "LibraryTest.kt", Some(43));
        assert_eq!(location.path, "a/LibraryTest.kt");
        let path = strings(&["a.LibraryTest", "cannot lend twice"]);
        assert!(events.contains(&ReportEvent::Location { path, location }));
    }

    #[test]
    fn temporary_configurations_are_added_selected_capped_and_persisted() {
        let mut store = project_store();
        let apps: Vec<RunConfig> =
            (0..MAX_TEMPORARY + 2).map(|i| app(&format!("a.App{i}"), "src/main/java/a/App.java")).collect();
        let mut persisted = Persist::default();
        for config in &apps {
            persisted = store.adopt_temporary(config.clone(), &[]).1;
        }
        assert_eq!(store.temporary.len(), MAX_TEMPORARY);
        assert!(!store.temporary.contains(&apps[0].id));
        assert_eq!(store.selected_id.as_ref(), apps.last().map(|config| &config.id));
        assert_eq!(store.configs().iter().filter(|config| config.kind == "application").count(), MAX_TEMPORARY);
        let stored = persisted.writes.iter().find(|write| write.key == "openidea.run.temporary.v2:/p");
        let stored = stored.and_then(|write| write.value.as_deref());
        assert_eq!(parse_temporary(stored).len(), MAX_TEMPORARY);
        assert_eq!(persisted.strip_derived.map(|strip| strip.configs.len()), Some(MAX_TEMPORARY));
        // Reusing an existing temporary moves it last instead of duplicating it.
        store.adopt_temporary(apps[3].clone(), &[]);
        assert_eq!(store.temporary.last(), Some(&apps[3].id));
        assert_eq!(store.configs().iter().filter(|config| config.id == apps[3].id).count(), 1);
    }

    #[test]
    fn temporary_configurations_reuse_a_saved_configuration_with_the_same_id() {
        let mut store = project_store();
        let saved = RunConfig { name: "Mine".into(), detected: false, ..cfg("application:a.Main", "application") };
        store.set_configs(vec![saved]);
        let (used, _) = store.adopt_temporary(app("a.Main", "src/main/java/a/Main.java"), &[]);
        assert_eq!(used.name, "Mine");
        assert!(store.temporary.is_empty());
    }

    #[test]
    fn temporary_configurations_reuse_a_configuration_of_the_same_main_class_and_module_whatever_its_id() {
        let own = |module: &str| {
            let mut config = cfg(&format!("user:{module}"), "spring-boot");
            config.name = format!("Mine {module}");
            config.args = strings(&["-Xmx1g", "-cp", "${classpath}", "a.Main"]);
            config.extras.module = Some(module.to_string());
            config.detected = false;
            config
        };
        let mut store = project_store();
        store.set_configs(vec![own("other"), own("app")]);
        let (used, _) = store.adopt_temporary(app("a.Main", "app/src/main/java/a/Main.java"), &[]);
        assert_eq!(used.name, "Mine app");
        assert!(store.temporary.is_empty());
        let (fresh, _) = store.adopt_temporary(app("a.Other", "app/src/main/java/a/Other.java"), &[]);
        assert_eq!(store.temporary, [fresh.id]);
    }

    #[test]
    fn saving_a_temporary_configuration_makes_it_a_user_configuration() {
        let mut store = project_store();
        let (adopted, _) = store.adopt_temporary(app("a.Main", "src/main/java/a/Main.java"), &[]);
        let (_, user) = store.save_temporary(&adopted.id).expect("a temporary configuration");
        assert!(store.temporary.is_empty());
        assert_eq!(
            user.iter().map(|config| (config.id.as_str(), config.detected)).collect::<Vec<_>>(),
            [(adopted.id.as_str(), false)]
        );
        assert_eq!(store.save_temporary(&adopted.id), None);
    }

    // Not from a spec: a busy temporary configuration is not the one evicted, and a removed file takes
    // its context configurations along.
    #[test]
    fn eviction_spares_running_configurations_and_stale_context_configurations_go() {
        let mut store = project_store();
        let context = |path: &str| cfg(&format!("context:file:{path}"), "python");
        let first = context("a.py#main");
        store.adopt_temporary(first.clone(), &[]);
        for i in 0..MAX_TEMPORARY {
            store.adopt_temporary(context(&format!("f{i}.py")), std::slice::from_ref(&first.id));
        }
        assert!(store.temporary.contains(&first.id));
        assert!(!store.temporary.contains(&"context:file:f0.py".to_string()));
        store.select_config(Some(&first.id));
        let persist = store.drop_stale_context_configs(&strings(&["/p/a.py"]));
        assert!(!store.temporary.contains(&first.id));
        assert_eq!(store.selected_id, None);
        assert!(persist.writes.iter().any(|write| write.key == "openidea.run.selected:/p" && write.value.is_none()));
    }

    // Not from a spec: pinned and recent ids are stored per project, and an empty list removes the key.
    #[test]
    fn pinned_and_recent_ids_are_stored_per_project() {
        let mut store = project_store();
        let pinned = store.toggle_pinned("a").expect("a project is open");
        assert_eq!((pinned.key.as_str(), pinned.value.as_deref()), ("openidea.run.pinned:/p", Some("[\"a\"]")));
        assert_eq!(store.toggle_pinned("a").and_then(|write| write.value), None);
        for id in ["1", "2", "3", "4", "5", "6", "2"] {
            store.mark_recent(id);
        }
        assert_eq!(store.recent, ["2", "6", "5", "4", "3"]);
        assert_eq!(store.mark_recent("2"), None);
        assert_eq!(store.mark_recent(CURRENT_FILE), None);
        assert_eq!(parse_ids(Some("[\"a\", 1, \"b\"]")), ["a", "b"]);
        assert!(parse_ids(Some("{}")).is_empty());
    }

    // Not from a spec: a reload keeps a stored selection that still exists and falls back to the default.
    #[test]
    fn loading_restores_the_selection_and_the_temporary_configurations() {
        let mut store = project_store();
        let saved = vec![cfg("npm:build", "npm"), cfg("npm:dev", "npm")];
        let temporary = serde_json::to_string(&[cfg("context:file:a.py", "python"), cfg("npm:dev", "npm")]).ok();
        let kept = RunStore::kept_temporary(&saved, temporary.as_deref());
        assert_eq!(kept.iter().map(|config| config.id.as_str()).collect::<Vec<_>>(), ["context:file:a.py"]);
        store.loaded(saved.clone(), kept.clone(), Some("gone"), None, Some("[\"npm:dev\"]"));
        assert_eq!(store.selected_id.as_deref(), Some("npm:dev"));
        assert_eq!(store.temporary, ["context:file:a.py"]);
        assert_eq!(store.recent, ["npm:dev"]);
        store.selected_id = None;
        store.loaded(saved, kept, Some("context:file:a.py"), None, None);
        assert_eq!(store.selected_id.as_deref(), Some("context:file:a.py"));
    }

    /// What a start asked the backend for, and how it ended.
    #[derive(Default)]
    struct Started {
        session: Option<String>,
        builds: Vec<BuildRequest>,
        searched: Vec<String>,
        resolved: Vec<(String, bool, BuildPolicy)>,
        launched: Vec<RunConfig>,
        effects: Vec<StartEffect>,
    }

    fn launch_info(build: JavaBuildStatus) -> JavaLaunchInfo {
        let failed = build != JavaBuildStatus::Succeeded;
        let error =
            JavaCompileError { path: "/p/app/src/Main.java".into(), line: 1, column: 1, message: "boom".into() };
        JavaLaunchInfo {
            build: Some(build),
            class_paths: strings(&["/p/app/target/classes"]),
            path_list_separator: ":".into(),
            java_exec: Some("java".into()),
            errors: if failed { vec![error] } else { Vec::new() },
            error_count: u32::from(failed),
            ..JavaLaunchInfo::default()
        }
    }

    /// Starts `config`, answering every step: the documents save, `Main.java` is found in the module
    /// `app`, jdtls answers with `resolve`, and the process host starts run 1.
    fn start_with(
        store: &mut RunStore,
        config: &RunConfig,
        options: StartOptions,
        types: &RunConfigTypes,
        resolve: Result<JavaLaunchInfo, IdeError>,
    ) -> Started {
        let settings = RunSettings::default();
        let env = StartEnv { types, settings: &settings };
        let mut i18n = I18n::default();
        let mut started = Started::default();
        let (mut start, mut step) = StartRun::start(config, options, store, env);
        loop {
            step = match step {
                StartStep::Build(request) => {
                    started.builds.push(request);
                    break;
                }
                StartStep::BeforeLaunch { .. } => start.before_launch_done(true, store, env),
                StartStep::StartChildren { .. } => break,
                StartStep::ConfirmRerun { .. } => start.rerun_confirmed(true, store),
                StartStep::StopAndWait { .. } => start.instances_stopped(),
                StartStep::SaveAll => start.saved(store, env, 10.0),
                StartStep::GradleDelegation(_) => start.delegation_resolved(Ok(None), store, env, 10.0, &mut i18n),
                StartStep::PrepareJava(first) => {
                    let mut java = first;
                    let outcome = loop {
                        let Some(preparation) = start.preparation() else { panic!("a preparation runs") };
                        java = match java {
                            PrepareStep::SaveAll => preparation.saved(Ok(()), &mut i18n),
                            PrepareStep::FindFiles { names, .. } => {
                                started.searched.extend(names.iter().cloned());
                                let found = names.iter().filter(|name| *name == "Main.java").map(|_| FoundSource {
                                    path: "/p/app/src/main/java/a/Main.java".into(),
                                    rel_path: "app/src/main/java/a/Main.java".into(),
                                });
                                preparation.files_found(found.collect(), &mut i18n)
                            }
                            PrepareStep::ListConfigs => preparation.configs_listed(Vec::new(), &mut i18n),
                            PrepareStep::ReadSources { paths } => {
                                let texts = vec!["package a;\npublic class Main {}".to_string(); paths.len()];
                                preparation.sources_read(&texts, &[], &mut i18n)
                            }
                            PrepareStep::ResolveLaunch { module, test_classpath, policy } => {
                                started.resolved.push((module, test_classpath, policy));
                                preparation.launch_resolved(resolve.clone(), &mut i18n)
                            }
                            PrepareStep::Done(prepared) => break Ok(*prepared),
                            PrepareStep::Failed(error) => break Err(error),
                        };
                    };
                    start.java_prepared(outcome, store, env, 10.0)
                }
                StartStep::PrepareType(launch) => start.type_prepared(Ok(*launch), store, 10.0, &mut i18n),
                StartStep::FinalizeLaunch(launch) => start.launch_finalized(Ok(*launch), store, 10.0, &mut i18n),
                StartStep::RunStart(launch) => {
                    let info = RunInfo {
                        id: RunId(1),
                        config_id: launch.id.clone(),
                        name: launch.name.clone(),
                        pid: Some(1),
                        running: true,
                        pty: false,
                    };
                    started.launched.push(*launch);
                    start.process_started(Ok(info), store, 10.0, &mut i18n)
                }
                StartStep::Done(session) => {
                    started.session = session;
                    break;
                }
            };
        }
        started.effects = start.take_effects();
        started
    }

    fn start(store: &mut RunStore, config: &RunConfig) -> Started {
        let types = RunConfigTypes::new();
        start_with(store, config, StartOptions::default(), &types, Ok(launch_info(JavaBuildStatus::Succeeded)))
    }

    fn status(store: &RunStore, started: &Started) -> Option<SessionStatus> {
        started.session.as_deref().and_then(|id| store.session(id)).map(|session| session.status)
    }

    /// An Application configuration of `a.Main`, as saved by hand.
    fn java_config() -> RunConfig {
        RunConfig {
            command: "java".into(),
            args: strings(&["-cp", "${classpath}", "a.Main"]),
            cwd: to_cwd(Some("app")),
            debug_adapter: Some("java-debug".into()),
            ..blank_config("user:main", "Main", "application")
        }
    }

    fn in_module(module: &str) -> RunConfig {
        let mut config = java_config();
        config.extras.module = Some(module.to_string());
        config
    }

    #[test]
    fn sends_explicit_configure_and_build_configurations_to_build_without_creating_run_tabs() {
        let mut store = RunStore::new();
        let cmake = |id: &str, args: &[&str]| RunConfig { args: strings(args), ..cfg(id, "cmake") };
        let builds = [
            cmake("cmake:build:configure", &["-S", ".", "-B", "build"]),
            cmake("cmake:build:build", &["--build", "build"]),
            cmake("cmake:release:hello:build", &["--build", "release", "--target", "hello"]),
        ];
        for config in &builds {
            let started = start(&mut store, config);
            assert_eq!(started.session, None);
            assert_eq!(started.builds, [build_cmake(&config.id, true)]);
        }
        assert!(store.sessions.is_empty());
    }

    #[test]
    fn runs_from_another_directory_using_the_stored_modules_classpath() {
        for cwd in ["sibling", "/outside/modules"] {
            let mut store = RunStore::new();
            let mut config = in_module("app");
            config.cwd = to_cwd(Some(cwd));
            let started = start(&mut store, &config);
            assert_eq!(status(&store, &started), Some(SessionStatus::Running));
            assert_eq!(started.resolved, [("app".to_string(), false, BuildPolicy::Automatic)]);
            assert_eq!(cwd_of(&started.launched[0]), Some(cwd));
            assert_eq!(started.launched[0].args, ["-cp", "/p/app/target/classes", "a.Main"]);
            assert!(started.searched.is_empty());
        }
    }

    #[test]
    fn passes_the_build_policy_to_the_resolver() {
        for policy in [BuildPolicy::NoBuild, BuildPolicy::Rebuild] {
            let mut store = RunStore::new();
            let mut config = in_module("app");
            config.extras.build_policy = Some(policy);
            assert_eq!(start(&mut store, &config).resolved, [("app".to_string(), false, policy)]);
        }
    }

    #[test]
    fn always_applies_the_launch_policy_after_a_before_launch_build_project() {
        let mut store = project_store();
        let started = start(&mut store, &in_module("app"));
        assert_eq!(started.resolved, [("app".to_string(), false, BuildPolicy::Automatic)]);
    }

    #[test]
    fn does_not_start_a_configuration_whose_build_failed() {
        let mut store = RunStore::new();
        let mut config = in_module("app");
        config.extras.build_policy = Some(BuildPolicy::Make);
        let types = RunConfigTypes::new();
        let failed = Ok(launch_info(JavaBuildStatus::Failed));
        let started = start_with(&mut store, &config, StartOptions::default(), &types, failed);
        assert_eq!(status(&store, &started), Some(SessionStatus::Failed));
        assert!(started.launched.is_empty());
    }

    #[test]
    fn starts_with_the_last_compiled_classes_for_make_and_continue_on_errors() {
        let mut store = RunStore::new();
        let mut config = in_module("app");
        config.extras.build_policy = Some(BuildPolicy::MakeAndContinueOnErrors);
        let types = RunConfigTypes::new();
        let with_errors = Ok(launch_info(JavaBuildStatus::WithErrors));
        let started = start_with(&mut store, &config, StartOptions::default(), &types, with_errors);
        assert_eq!(status(&store, &started), Some(SessionStatus::Running));
        assert_eq!(started.launched.len(), 1);
    }

    #[test]
    fn does_not_launch_scoped_compilation_error_bytecode() {
        for policy in [None, Some(BuildPolicy::Automatic), Some(BuildPolicy::Make), Some(BuildPolicy::Rebuild)] {
            let mut store = RunStore::new();
            let mut config = in_module("app");
            config.extras.build_policy = policy;
            let types = RunConfigTypes::new();
            let with_errors = Ok(launch_info(JavaBuildStatus::WithErrors));
            let started = start_with(&mut store, &config, StartOptions::default(), &types, with_errors);
            assert_eq!(status(&store, &started), Some(SessionStatus::Failed), "{policy:?}");
            assert!(started.launched.is_empty());
        }
    }

    #[test]
    fn finds_the_source_module_for_an_old_saved_configuration_without_a_module() {
        let mut store = RunStore::new();
        let mut config = java_config();
        config.cwd = to_cwd(Some("/outside/modules"));
        let started = start(&mut store, &config);
        assert!(started.searched.contains(&"Main.java".to_string()));
        assert_eq!(started.resolved, [("app".to_string(), false, BuildPolicy::Automatic)]);
        assert_eq!(cwd_of(&started.launched[0]), Some("/outside/modules"));
    }

    #[test]
    fn colours_spring_boot_runs_without_classpath_resolution() {
        let classpaths: [&[&str]; 2] = [&["-cp", "lib/*"], &[]];
        for classpath in classpaths {
            let mut store = RunStore::new();
            let program = strings(&["a.Main", "--spring.profiles.active=dev"]);
            let args = [strings(&["-Xmx256m"]), strings(classpath), program].concat();
            let boot = RunConfig { kind: "spring-boot".into(), args: args.clone(), ..java_config() };
            let started = start(&mut store, &boot);
            assert_eq!(status(&store, &started), Some(SessionStatus::Running));
            let expected = [strings(&["-Dspring.output.ansi.enabled=always"]), args.clone()].concat();
            assert_eq!(started.launched[0].args, expected);
            assert_eq!(started.resolved, [("app".to_string(), false, BuildPolicy::Automatic)]);
            assert_eq!(boot.args, args);
        }
    }

    #[test]
    fn preserves_an_explicit_spring_boot_console_setting() {
        for value in ["never", "detect", "always"] {
            let mut store = RunStore::new();
            let console = format!("-Dspring.output.ansi.enabled={value}");
            let args = strings(&[console.as_str(), "-cp", "lib/*", "a.Main"]);
            let boot = RunConfig { kind: "spring-boot".into(), args: args.clone(), ..java_config() };
            let started = start(&mut store, &boot);
            assert_eq!(started.launched[0].args, args);
            assert_eq!(started.resolved, [("app".to_string(), false, BuildPolicy::Automatic)]);
        }
    }

    #[test]
    fn keeps_plain_java_runs_with_an_explicit_classpath_free_of_spring_boot_options() {
        let mut store = RunStore::new();
        let args = strings(&["-cp", "lib/*", "a.Main"]);
        let started = start(&mut store, &RunConfig { args: args.clone(), ..java_config() });
        assert_eq!(started.launched[0].args, args);
        assert_eq!(started.resolved, [("app".to_string(), false, BuildPolicy::Automatic)]);
    }

    #[test]
    fn adds_the_spring_boot_console_option_once_when_resolving_the_module_classpath() {
        let mut store = RunStore::new();
        let boot = RunConfig { kind: "spring-boot".into(), ..in_module("app") };
        let started = start(&mut store, &boot);
        let expected = ["-Dspring.output.ansi.enabled=always", "-cp", "/p/app/target/classes", "a.Main"];
        assert_eq!(started.launched[0].args, expected);
    }

    #[test]
    fn stores_the_source_module_when_a_manually_created_application_is_saved() {
        let mut config = java_config();
        config.cwd = to_cwd(Some("sibling"));
        let (mut save, step) = SaveConfigs::start(vec![config], Vec::new());
        let SaveStep::Module(ModuleStep::FindFiles { names, limit: 1000 }) = step else { panic!("a file search") };
        assert_eq!(names[0], "Main.java");
        let found = FoundSource {
            path: "/p/app/src/main/java/a/Main.java".into(),
            rel_path: "app/src/main/java/a/Main.java".into(),
        };
        assert_eq!(save.files_found(vec![found]), SaveStep::Module(ModuleStep::ListConfigs));
        assert!(matches!(save.configs_listed(Vec::new()), SaveStep::Module(ModuleStep::ReadSources { .. })));
        let SaveStep::Save { configs, removed_detected_ids } =
            save.sources_read(&["package a;\npublic class Main {}".to_string()], &[])
        else {
            panic!("the configurations are saved")
        };
        assert_eq!(configs[0].extras.module.as_deref(), Some("app"));
        assert_eq!(cwd_of(&configs[0]), Some("sibling"));
        assert!(removed_detected_ids.is_empty());
    }

    #[test]
    fn explains_how_to_import_a_module_when_run_fails() {
        for message in [
            "jdtls: Given URI does not belong to any Java project.",
            "jdtls: Launch configuration 1793460461218 references non-existing project parent.",
        ] {
            let mut store = RunStore::new();
            let types = RunConfigTypes::new();
            let error = Err(IdeError::new(ErrorKind::Protocol, message));
            let started = start_with(&mut store, &in_module("web"), StartOptions::default(), &types, error);
            assert_eq!(status(&store, &started), Some(SessionStatus::Failed));
            let output = started.session.as_deref().and_then(|id| store.console(id)).map(ConsoleModel::text);
            let output = output.unwrap_or_default();
            assert!(output.contains("Java module 'web' is not imported."), "{output}");
            assert!(output.contains("parent Maven or Gradle build"));
            assert!(output.contains("open its folder as a project"));
            assert!(started.launched.is_empty());
        }
    }

    // Not from a spec: a configuration that only attaches a debugger tells so instead of starting.
    #[test]
    fn a_debug_only_configuration_offers_debug_instead_of_running() {
        let mut store = RunStore::new();
        let mut types = RunConfigTypes::new();
        types.register(RunConfigType::new("jvm-remote", "Remote JVM Debug", "debug", |_, _| None).debug_only());
        let ok = Ok(launch_info(JavaBuildStatus::Succeeded));
        let started = start_with(&mut store, &cfg("remote", "jvm-remote"), StartOptions::default(), &types, ok);
        assert_eq!(started.session, None);
        let [StartEffect::Notify(request)] = &started.effects[..] else { panic!("one notification") };
        assert_eq!(request.title, "'remote' can only be debugged");
        assert_eq!(request.actions[0].command, "run.debugConfig:remote");
    }

    // Not from a spec: a second start of a running configuration asks, stops it and reuses its tab.
    #[test]
    fn restarting_a_running_configuration_stops_it_and_reuses_its_tab() {
        let mut store = RunStore::new();
        let config = cfg("npm:dev", "npm");
        let first = start(&mut store, &config).session.expect("a session");
        let types = RunConfigTypes::new();
        let settings = RunSettings::default();
        let env = StartEnv { types: &types, settings: &settings };
        let (mut again, step) = StartRun::start(&config, StartOptions::default(), &mut store, env);
        assert!(matches!(step, StartStep::ConfirmRerun { ref name, count: 1 } if name == "npm:dev"));
        let StartStep::StopAndWait { session_ids } = again.rerun_confirmed(true, &mut store) else {
            panic!("the running instance is stopped first")
        };
        assert_eq!(session_ids, std::slice::from_ref(&first));
        assert_eq!(store.stop_session(&first, 20.0), Some(StopAction::StopRun(RunId(1))));
        assert!(store.any_live(&session_ids));
        assert_eq!(store.on_run_exited(RunId(1), Some(143)).as_deref(), Some(first.as_str()));
        let finished = store.finish_session(&first, Some(0), Some(143), 30.0, FinishContext::default());
        // Stopped by the user: no failure notification.
        assert_eq!((finished.notification, finished.exited), (None, Some((Some(143), true))));
        assert!(!store.any_live(&session_ids));
        assert!(matches!(again.instances_stopped(), StartStep::SaveAll));
        again.saved(&mut store, env, 40.0);
        assert_eq!(again.session_id(), Some(first.as_str()));
        assert_eq!((store.sessions.len(), again.generation()), (1, 1));
        assert_eq!(store.recent.len(), 0);
    }

    // Not from a spec: a failed run tells so when the Run tool window is hidden, with Show and Rerun.
    #[test]
    fn a_failed_run_notifies_when_the_run_window_is_hidden() {
        let mut store = RunStore::new();
        let session = start(&mut store, &cfg("npm:build", "npm")).session.expect("a session");
        let hidden = FinishContext::default();
        let finished = store.finish_session(&session, Some(0), Some(2), 50.0, hidden);
        let request = finished.notification.expect("a notification");
        assert_eq!(request.title, "'npm:build' exited with code 2");
        assert_eq!(request.actions.iter().map(|action| action.label.as_str()).collect::<Vec<_>>(), ["Show", "Rerun"]);
        let again = start(&mut store, &cfg("npm:test", "npm")).session.expect("a session");
        let tests = FinishContext { failed_tests: 3, ..FinishContext::default() };
        let failed = store.finish_session(&again, Some(0), Some(1), 60.0, tests).notification.expect("a notification");
        assert_eq!((failed.title.as_str(), failed.body.as_deref()), ("Tests failed: 3", Some("npm:test")));
        let visible = FinishContext { run_window_visible: true, ..FinishContext::default() };
        let quiet = start(&mut store, &cfg("npm:lint", "npm")).session.expect("a session");
        assert_eq!(store.finish_session(&quiet, Some(0), Some(1), 70.0, visible).notification, None);
    }

    #[test]
    fn shows_a_flood_of_output_at_most_every_50_ms_a_first_line_on_the_next_frame() {
        const FRAME_MS: f64 = 16.0;
        let mut store = RunStore::new();
        let id = store.start_sample_session(0.0);
        // Creating the session is the first update.
        let mut updates = vec![0.0];
        let (lines, per_frame) = (600, 10);
        let (mut next, mut now) = (0, 0.0);
        let (mut frame_flush, mut timer): (bool, Option<f64>) = (false, None);
        while next < lines || frame_flush || timer.is_some() {
            now += FRAME_MS;
            // A timer that ran out since the last frame fires first; its flush waits for this frame.
            if let Some(at) = timer.filter(|at| *at <= now) {
                timer = None;
                match store.flush_timer_fired(&id, at, false) {
                    Ok(flushed) if flushed.changed => updates.push(at),
                    Ok(_) => {}
                    Err(_) => frame_flush = true,
                }
            }
            if std::mem::take(&mut frame_flush) && store.flush(&id, now).changed {
                updates.push(now);
            }
            // 60 frames of output, 10 lines each.
            for _ in 0..per_frame.min(lines - next) {
                let (kind, text) = sample_line(next);
                match store.enqueue(&id, kind, &text, now, false).schedule {
                    Some(FlushSchedule::Frame) => frame_flush = true,
                    Some(FlushSchedule::Timeout(wait)) => timer = Some(now + wait),
                    None => {}
                }
                next += 1;
            }
        }
        now += FRAME_MS;
        if store.flush(&id, now).changed {
            updates.push(now);
        }
        let generation = store.generation(&id);
        let visible = FinishContext { run_window_visible: true, ..FinishContext::default() };
        store.finish_session(&id, generation, Some(0), now, visible);
        assert_ne!(store.session(&id).map(|session| session.status), Some(SessionStatus::Running));
        assert_eq!(store.console(&id).map(ConsoleModel::size), Some(lines));
        // The first lines (enqueued a frame later) show on the next frame; then every ≥ 50 ms.
        assert!(updates[1] - updates[0] <= 34.0, "{updates:?}");
        let gaps: Vec<f64> = updates.windows(2).skip(1).map(|pair| pair[1] - pair[0]).collect();
        let steady = &gaps[..gaps.len().saturating_sub(2)];
        assert!(steady.iter().all(|gap| *gap >= FLUSH_INTERVAL_MS), "{updates:?}");
        assert!(updates.len() < 30, "{updates:?}");
    }

    // Not from a spec: a hidden window flushes on a timeout, and returning to it moves the flush to a frame.
    #[test]
    fn flushes_follow_the_window_visibility() {
        let mut store = RunStore::new();
        let id = store.start_sample_session(0.0);
        let hidden = store.enqueue(&id, ChunkKind::Stdout, "a\n", 5.0, true);
        assert_eq!(hidden.schedule, Some(FlushSchedule::Timeout(HIDDEN_FLUSH_DELAY_MS)));
        assert_eq!(store.reschedule_console_flushes(false), [(id.clone(), FlushSchedule::Frame)]);
        assert!(store.reschedule_console_flushes(false).is_empty());
        let flushed = store.flush(&id, 21.0);
        assert_eq!(flushed.lines, [CompletedLine { text: "a".into(), kind: ChunkKind::Stdout }]);
        let soon = store.enqueue(&id, ChunkKind::Stdout, "b\n", 31.0, false);
        assert_eq!(soon.schedule, Some(FlushSchedule::Timeout(40.0)));
        assert_eq!(store.flush_timer_fired(&id, 71.0, false), Err(FlushSchedule::Frame));
        assert_eq!(store.flush_timer_fired(&id, 71.0, true).map(|flushed| flushed.changed), Ok(true));
    }

    // Not from a spec: a server start-up line gives the session its HTTP port.
    #[test]
    fn reads_the_http_port_of_a_started_server() {
        assert_eq!(started_port("Tomcat started on port 8080 (http) with context path '/'"), Some(8080));
        assert_eq!(started_port("Undertow started on port(s) 9090 (http)"), Some(9090));
        assert_eq!(started_port("Tomcat started on port 0"), None);
        assert_eq!(started_port("Started on port 8080"), None);
        let mut store = RunStore::new();
        let id = store.start_sample_session(0.0);
        store.enqueue(&id, ChunkKind::Stdout, "Netty started on port 8081\n", 100.0, false);
        store.flush(&id, 116.0);
        assert_eq!(store.session(&id).and_then(|session| session.http_port), Some(8081));
    }

    // Not from a spec: PHP and Node.js tools run on the target their language settings choose.
    #[test]
    fn language_run_targets_apply_to_their_tools() {
        let settings = RunSettings {
            default_target: "box".into(),
            php_target: "local".into(),
            node_target: "docker".into(),
            ..RunSettings::default()
        };
        let command = |command: &str| RunConfig { command: command.into(), ..cfg("c", "shell") };
        assert_eq!(with_run_target(&command("C:\\php\\php.EXE"), &settings).extras.target.as_deref(), Some(""));
        assert_eq!(with_run_target(&command("/usr/bin/pnpm"), &settings).extras.target.as_deref(), Some("docker"));
        assert_eq!(with_run_target(&command("cargo"), &settings).extras.target.as_deref(), Some("box"));
        let mut chosen = command("cargo");
        chosen.extras.target = Some("mine".into());
        assert_eq!(with_run_target(&chosen, &settings).extras.target.as_deref(), Some("mine"));
    }
}
