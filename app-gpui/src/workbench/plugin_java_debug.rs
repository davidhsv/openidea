//! A plugin's debug request for a configuration whose run detector names the Java debugger with
//! `debugArgs` (Bazel's `--java_debug`, say): the configuration runs in the Run tool window with
//! those arguments, its JVM waits for a debugger on a JDWP port, and once the JVM announces that
//! port ("Listening for transport dt_socket at address: N") the Java debugger attaches to it on
//! loopback. The debug session belongs to the run's process, so Stop in the debugger ends both;
//! the plugin hears the process's exit through its run operation, which the backend reports.
//!
//! The output is read from the run's console as it is shown: each completed line of the process
//! (not the IDE's own messages, not a line still being written) goes through
//! [`AgentDebug::on_line`]. A port that is not a TCP port is ignored.

use super::Workbench;
use super::plugin_contributions::RUN_NOT_STARTED;
use super::plugin_runtime::answer_plugin_command;
use crate::run::console::ChunkKind;
use crate::run::java_debug::{AgentDebug, AttachRequest};
use crate::run::store::{RunSession, RunStore, SessionStatus, StartOptions};
use gpui_kit::{Context, Entity, Subscription, Window};
use ide_host::plugins::CommandAnswer;
use serde_json::{Value, json};
use std::collections::HashMap;

/// The debugger's attach to a JVM listening for JDWP; it is not a registered action.
const ATTACH: &str = "debug.attach";
/// The only host the debugger attaches to: the JVM runs on this machine.
const LOOPBACK: &str = "127.0.0.1";

/// The plugin debug runs whose JVM the debugger attaches to, by the plugin's operation.
#[derive(Default)]
pub(super) struct JavaDebugRuns {
    runs: HashMap<String, JavaDebugRun>,
}

/// One run with a JDWP agent: what reads its output, and how far it has read.
struct JavaDebugRun {
    agent: AgentDebug,
    /// The Run tab's session and the start of it that is followed, once the run started.
    session: Option<(String, u64)>,
    /// The id of the last console line read.
    seen: u64,
    _output: Subscription,
}

impl Workbench {
    /// A plugin asks to debug `config` for its operation `operation`: when the configuration starts
    /// a JVM with a waiting JDWP agent (its detector's `debugArgs` with the Java debugger), it runs
    /// with them and the debugger attaches to every port the JVM announces. False when this is not
    /// such a configuration, or the window has no project to run and debug it in.
    pub(super) fn plugin_java_debug_request(
        &mut self,
        request_id: &str,
        operation: &str,
        config: &ide_run::RunConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (Some(run), true) = (self.run_driver().cloned(), self.debug_driver().is_some()) else { return false };
        let Some((agent, launch)) = AgentDebug::start(config, None, None) else { return false };
        let options = StartOptions { launch: Some(launch), ..StartOptions::default() };
        let started = run.update(cx, |run, cx| run.start(config.clone(), options, cx));
        let store = run.read(cx).store().clone();
        let output = {
            let operation = operation.to_owned();
            cx.observe_in(&store, window, move |workbench, store, window, cx| {
                workbench.java_debug_output(&operation, &store, window, cx);
            })
        };
        let run = JavaDebugRun { agent, session: None, seen: 0, _output: output };
        self.plugin_contributions.java_debug.runs.insert(operation.to_owned(), run);
        let (request, operation) = (request_id.to_owned(), operation.to_owned());
        cx.spawn_in(window, async move |workbench, cx| {
            let session = started.await;
            let _ = workbench.update_in(cx, |workbench, window, cx| {
                workbench.java_debug_started(&request, &operation, session, &store, window, cx);
            });
        })
        .detach();
        true
    }

    /// The run of `operation` started (or did not): the plugin waiting on `request` hears so, and
    /// the output so far is read with the process known.
    fn java_debug_started(
        &mut self,
        request: &str,
        operation: &str,
        session: Option<String>,
        store: &Entity<RunStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let started = {
            let store = store.read(cx);
            session
                .and_then(|id| store.session(&id).cloned())
                .filter(|session| session.status != SessionStatus::Failed)
                .map(|session| {
                    let generation = store.generation(&session.id).unwrap_or_default();
                    (session, generation)
                })
        };
        let Some((session, generation)) = started else {
            self.plugin_contributions.java_debug.runs.remove(operation);
            return self.refuse_plugin_run(request, operation, RUN_NOT_STARTED, cx);
        };
        if !request.is_empty() {
            answer_plugin_command(&self.label, request, CommandAnswer::Value(None), cx);
        }
        let Some(run) = self.plugin_contributions.java_debug.runs.get_mut(operation) else { return };
        run.session = Some((session.id.clone(), generation));
        let attaches = run.agent.session_started(session.run_id);
        self.java_debug_attach(attaches, window, cx);
        self.java_debug_output(operation, store, window, cx);
    }

    /// The Run tool window's sessions changed: the lines the run of `operation` completed since the
    /// last look are read for a JDWP port. Once its session has ended (or its tab was restarted) the
    /// run is no longer followed.
    fn java_debug_output(
        &mut self,
        operation: &str,
        store: &Entity<RunStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = self.plugin_contributions.java_debug.runs.get_mut(operation) else { return };
        let Some((session, generation)) = run.session.clone() else { return };
        let (lines, followed) = {
            let store = store.read(cx);
            let followed = store.session(&session).is_some_and(RunSession::is_live)
                && store.generation(&session) == Some(generation);
            let lines: Vec<(u64, ChunkKind, String)> = store
                .console(&session)
                .map(|console| {
                    let open = console.open_line().map(|line| line.id);
                    let all = console.lines();
                    let from = all.partition_point(|line| line.id <= run.seen);
                    all[from..]
                        .iter()
                        .filter(|line| Some(line.id) != open)
                        .map(|line| (line.id, line.kind, line.text.clone()))
                        .collect()
                })
                .unwrap_or_default();
            (lines, followed)
        };
        let mut attaches = Vec::new();
        for (id, kind, text) in lines {
            run.seen = id;
            if kind != ChunkKind::System {
                attaches.extend(run.agent.on_line(&text));
            }
        }
        if !followed {
            self.plugin_contributions.java_debug.runs.remove(operation);
        }
        self.java_debug_attach(attaches, window, cx);
    }

    /// Attaches the Java debugger to each JVM that announced its port, on loopback, as the session
    /// of the run's process (Stop in the debugger stops the process too).
    fn java_debug_attach(&mut self, attaches: Vec<AttachRequest>, window: &mut Window, cx: &mut Context<Self>) {
        for attach in attaches {
            let Some(port) = u16::try_from(attach.port).ok().filter(|port| *port != 0) else {
                tracing::debug!("a JVM announced the JDWP port {}, which is not a TCP port", attach.port);
                continue;
            };
            let run_config = match serde_json::to_value(&attach.run_config) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!("the debugger does not attach to '{}': {error}", attach.name);
                    continue;
                }
            };
            let mut arg = json!({
                "name": attach.name,
                "module": attach.module,
                "hostName": LOOPBACK,
                "port": port,
                "runConfig": run_config,
            });
            if let Some(process) = attach.process {
                arg["process"] = Value::from(process.0);
            }
            if let Some(show) = attach.show {
                arg["show"] = Value::Bool(show);
            }
            self.run_debug_action(ATTACH, Some(&arg), window, cx);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::plugins_window::test_support::{
        PluginHome, link, plugin_copy, program, programs_in, settle_app, trusted_project,
    };
    use super::super::tests::{notice_titled, notification_titles, run_action, wait_long};
    use super::*;
    use crate::backend::Backend;
    use crate::debugger::driver::JavaDebugServerPort;
    use crate::debugger::state::SessionStatus as DebugStatus;
    use crate::debugger::window_view::DebugWindowView;
    use crate::editor::marks::MarkLayer;
    use crate::plugins::{Plugins, plugin_action_id};
    use crate::theme;
    use gpui_kit::{TestAppContext, VisualTestContext};
    use ide_core::RunId;
    use ide_plugin::Invocation;
    use std::io::Write as _;
    use std::net::{Shutdown, TcpListener};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    const LABEL: &str = "w1";
    /// The configuration of the `execution` fixture's detector, and the Debug tab of its session.
    const DETECTED: &str = "Detected test";
    /// What the adapter answers an attach it refuses with, as java-debug does when no JVM listens.
    const REFUSED: &str =
        concat!("Failed to attach to remote debuggee VM. ", "Reason: java.net.ConnectException: Connection refused");
    /// The JDWP port the test JVM announces (nothing listens there: the adapter only records it).
    const JDWP_PORT: &str = "50123";

    /// The `execution` fixture's detector set to the Java debugger, started with `--java_debug`.
    fn java_detector(text: &str) -> String {
        let attaching = "framework = \"plugin\"\ndebugger = \"java-debug\"\ndebugArgs = [\"--java_debug\"]\n";
        let edited = text.replacen("framework = \"plugin\"\n", attaching, 1);
        assert_ne!(edited, text, "the fixture's detector names its test framework");
        edited
    }

    /// The program the detector runs, standing for a JVM started with a waiting JDWP agent: it
    /// prints its arguments, then JDWP's start-up line with each address in `jdwp.port`, one per
    /// line (when the project has that file), and exits with 3 once the project has a file
    /// `release` (with 4 after a minute without one, so a failed test leaves nothing running). Like
    /// a JVM it exits on SIGINT: without the trap a shell whose `sleep` ends just as the signal
    /// comes takes it as handled and goes on.
    const JVM: &str = "#!/bin/sh\n\
        trap 'exit 130' INT\n\
        echo \"execution-tool $*\"\n\
        if [ -f jdwp.port ]; then while read -r at || [ -n \"$at\" ]; do\n\
        \x20 echo \"Listening for transport dt_socket at address: $at\"; done < jdwp.port; fi\n\
        waited=0\n\
        while [ ! -f release ]; do\n\
        \x20 sleep 0.05; waited=$((waited + 1)); if [ \"$waited\" -gt 1200 ]; then exit 4; fi\n\
        done\n\
        exit 3\n";

    /// The fake adapter of the debugger's tests, able to attach: an attach runs `main.fake` as a
    /// launch would, after writing the host and port it was asked for to `attach.json`; with a file
    /// `refuse-attach` in its folder it refuses the attach with [`REFUSED`].
    fn attaching_adapter() -> String {
        let launch = "    elif command == 'launch':\n        launch = request\n        \
                      source = os.path.join(arguments.get('cwd') or '.', arguments['program'])\n";
        let attach = format!(
            "    elif command in ('launch', 'attach'):\n\
             \x20       if command == 'attach':\n\
             \x20           with open('attach.json', 'w') as seen:\n\
             \x20               json.dump({{key: arguments.get(key) for key in ('hostName', 'port')}}, seen)\n\
             \x20           if os.path.exists('refuse-attach'):\n\
             \x20               send({{'type': 'response', 'request_seq': request['seq'], 'command': command,\n\
             \x20                     'success': False, 'message': {REFUSED:?}}})\n\
             \x20               continue\n\
             \x20       launch = request\n\
             \x20       source = os.path.join(arguments.get('cwd') or '.', arguments.get('program') or 'main.fake')\n"
        );
        let fake = super::super::debug::tests::FAKE_ADAPTER;
        let adapter = fake.replacen(launch, &attach, 1);
        assert_ne!(adapter, fake, "the fake adapter's launch is where an attach goes");
        adapter
    }

    /// The Java debug server of a test: a loopback listener whose every connection runs the
    /// attaching adapter over the socket, in the project's folder. Its port is what the debugger
    /// gets for the debug server of jdtls.
    fn java_debug_server(adapter: PathBuf, folder: PathBuf) -> u16 {
        let listener = TcpListener::bind((LOOPBACK, 0)).expect("a loopback port");
        let port = listener.local_addr().expect("the listener is bound").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut child = Command::new("python3")
                    .arg(&adapter)
                    .current_dir(&folder)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("the adapter starts");
                let (mut input, mut output) =
                    (child.stdin.take().expect("the adapter's input"), child.stdout.take().expect("its output"));
                let mut from_debugger = stream.try_clone().expect("the socket is shared");
                let mut to_debugger = stream;
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut from_debugger, &mut input);
                    let _ = input.flush();
                });
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut output, &mut to_debugger);
                    let _ = to_debugger.shutdown(Shutdown::Both);
                    let _ = child.wait();
                });
            }
        });
        port
    }

    /// A temporary home whose languages include `fakedap` (`*.fake` files take breakpoints), with
    /// the backend and the plugin host running in it.
    fn start_home(cx: &mut TestAppContext) -> PluginHome {
        let hosts = crate::plugins::lock_test_hosts();
        cx.executor().allow_parking();
        let home = tempfile::tempdir().expect("a temporary home");
        let dirs = crate::backend::AppDirs::under(home.path());
        let script = home.path().join("fake_dap.py");
        std::fs::write(&script, super::super::debug::tests::FAKE_ADAPTER).expect("the adapter script is written");
        let languages = dirs.config.join("languages");
        std::fs::create_dir_all(&languages).expect("the languages folder is made");
        let toml = format!(
            "id = \"fakedap\"\nname = \"Fake\"\nextensions = [\"fake\"]\nline-comment = \"#\"\n\n\
             [debugger]\nadapter = \"fake-dap\"\ncommand = \"python3\"\nargs = [{:?}]\n",
            script.to_string_lossy(),
        );
        std::fs::write(languages.join("fakedap.toml"), toml).expect("the language is written");
        cx.update(|cx| {
            cx.set_global(theme::ActiveTheme::default());
            cx.set_global(crate::platform::storage::UiStorage::default());
            crate::platform::input::init(cx);
            crate::editor::actions::register(cx);
            crate::workbench::register_actions(cx);
            Backend::init(dirs, cx).expect("the backend starts");
            crate::workbench::menu::init(cx);
            crate::terminal::test_support::isolate_shells(home.path(), cx);
            theme::appearance::init(cx);
            Plugins::init(cx).expect("the plugin host starts");
        });
        settle_app(cx, "the first listing", |cx| cx.global::<Plugins>().snapshot().is_some());
        PluginHome::new(home, hosts)
    }

    /// What a test works with: the home, the project, its window.
    struct Fixture {
        _home: PluginHome,
        _folder: tempfile::TempDir,
        root: PathBuf,
        workbench: Entity<Workbench>,
    }

    /// The `execution` fixture with its detector edited by `edit`, granted the test JVM, in a
    /// trusted project of `files` with the detector's marker and `main.fake`; the window's debugger
    /// gets the test's Java debug server.
    fn project<'a>(
        cx: &'a mut TestAppContext,
        edit: impl Fn(&str) -> String,
        files: &[(&str, &str)],
    ) -> (Fixture, &'a mut VisualTestContext) {
        let home = start_home(cx);
        let bin = home.path().join("bin");
        program(&bin, "execution-tool", JVM);
        programs_in(&bin, cx);
        let folder = plugin_copy(home.path(), "execution", edit);
        link(cx, &folder, &["process:execution-tool"], "debug");
        let mut files = files.to_vec();
        files.extend([("execution.marker", ""), ("main.fake", "start\nmiddle\nend\n")]);
        let (folder, root, workbench, cx) = trusted_project(cx, LABEL, &files);
        let adapter = home.path().join("java_dap.py");
        std::fs::write(&adapter, attaching_adapter()).expect("the adapter script is written");
        let port = java_debug_server(adapter, root.clone());
        cx.update(|_, cx| cx.set_global(JavaDebugServerPort(port)));
        (Fixture { _home: home, _folder: folder, root, workbench }, cx)
    }

    impl Fixture {
        /// Debug of the detector's configuration, as the plugin's `debug` command asks for it.
        fn debug_detected(&self, cx: &mut VisualTestContext) {
            run_action(cx, &plugin_action_id("execution", "debug"), Some(Value::from("detected")));
        }

        fn run_session(&self, cx: &mut VisualTestContext) -> Option<RunSession> {
            let driver = self.workbench.read_with(cx, |w, _| w.run_driver().cloned()).expect("the project runs");
            driver.read_with(cx, |driver, cx| {
                driver.store().read(cx).sessions.iter().find(|session| session.config.name == DETECTED).cloned()
            })
        }

        fn run_lines(&self, cx: &mut VisualTestContext) -> Vec<String> {
            let Some(session) = self.run_session(cx) else { return Vec::new() };
            let driver = self.workbench.read_with(cx, |w, _| w.run_driver().cloned()).expect("the project runs");
            driver.read_with(cx, |driver, cx| {
                let store = driver.store().read(cx);
                let lines = store.console(&session.id).map(|console| console.lines().to_vec()).unwrap_or_default();
                lines.into_iter().map(|line| line.text).collect()
            })
        }

        /// The debugger's sessions: status and the run process each belongs to.
        fn debug_sessions(&self, cx: &mut VisualTestContext) -> Vec<(DebugStatus, Option<RunId>)> {
            let driver = self.workbench.read_with(cx, |w, _| w.debug_driver().cloned()).expect("a debugger");
            driver.read_with(cx, |driver, cx| {
                let state = &driver.debugger().read(cx).state;
                state.sessions().values().map(|session| (session.status, session.process)).collect()
            })
        }

        fn debug_view(&self, cx: &mut VisualTestContext) -> Option<Entity<DebugWindowView>> {
            self.workbench.read_with(cx, |w, _| w.debug_window_view().cloned())
        }

        fn debug_tabs(&self, cx: &mut VisualTestContext) -> Vec<String> {
            self.debug_view(cx).map(|view| view.update(cx, |view, cx| view.tab_titles(cx))).unwrap_or_default()
        }

        fn frame_lines(&self, cx: &mut VisualTestContext) -> Vec<String> {
            let Some(view) = self.debug_view(cx) else { return Vec::new() };
            let frames = view.read_with(cx, |view, _| view.frames().clone());
            frames.update(cx, |frames, cx| frames.frame_lines(cx))
        }

        /// The host and port the adapter was asked to attach to.
        fn attached_to(&self) -> Option<Value> {
            let text = std::fs::read_to_string(self.root.join("attach.json")).ok()?;
            serde_json::from_str(&text).ok()
        }

        /// Lets the test JVM exit with 3.
        fn release(&self) {
            std::fs::write(self.root.join("release"), "").expect("the file is written");
        }

        /// The data of the `run-exited` event the plugin heard, once it did.
        fn heard_exit(&self, cx: &mut VisualTestContext) -> Option<Value> {
            stored_events(cx, &self.root).into_iter().find_map(|event| {
                let heard = event["kind"] == "run-exited";
                heard.then(|| serde_json::from_str(event["data"].as_str().unwrap_or("{}")).ok()).flatten()
            })
        }
    }

    /// The `kind`-`data` events the `execution` fixture stored, as its `events` command lists them.
    fn stored_events(cx: &mut VisualTestContext, root: &Path) -> Vec<Value> {
        let host = cx.update(|_, cx| cx.global::<Plugins>().host.clone());
        let invocation = Invocation {
            command_chain: Vec::new(),
            client: Some(LABEL.to_owned()),
            workspace_root: Some(root.to_path_buf()),
            editor: None,
            selected_paths: Vec::new(),
            global_settings: false,
        };
        let running = std::thread::spawn(move || host.run_command("execution", "events", None, invocation));
        wait_long(cx, "the plugin lists its events", |_| running.is_finished());
        let listed = running.join().expect("the command ran").ok().flatten();
        listed.and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok()).unwrap_or_default()
    }

    /// A plugin's Debug of its detector set to the Java debugger runs the detector's command with
    /// its debug arguments in the Run tool window; once the JVM announces its JDWP port the Java
    /// debugger attaches to it on loopback as the process's session, with the breakpoint set before
    /// (the session stops there), and the plugin hears the process's exit code.
    #[gpui_kit::test]
    async fn a_plugins_java_debug_request_attaches_to_the_port_the_jvm_announces(cx: &mut TestAppContext) {
        let (fixture, cx) = project(cx, java_detector, &[("jdwp.port", JDWP_PORT)]);
        let file = fixture.root.join("main.fake");
        fixture.workbench.update_in(cx, |w, window, cx| w.open_project_file(file.clone(), window, cx));
        wait_long(cx, "the editor", |cx| fixture.workbench.read_with(cx, |w, _| w.editor().is_some()));
        let editor = fixture.workbench.read_with(cx, |w, _| w.editor().cloned()).expect("an editor");
        editor.update(cx, |editor, cx| {
            let at = editor.offset_of(1, 0);
            editor.select(at, at, cx);
        });
        run_action(cx, "debug.toggleBreakpoint", None);
        wait_long(cx, "the breakpoint", |cx| editor.read_with(cx, |e, _| e.marks(MarkLayer::Breakpoints).len() == 1));

        fixture.debug_detected(cx);
        wait_long(cx, "the session in the Debug tool window", |cx| fixture.debug_tabs(cx) == [DETECTED]);
        wait_long(cx, "the stop at the breakpoint", |cx| fixture.frame_lines(cx) == ["main, main.fake:2"]);
        let attached = fixture.attached_to().expect("the adapter was asked to attach");
        assert_eq!(attached, json!({ "hostName": LOOPBACK, "port": 50123 }));
        let lines = fixture.run_lines(cx);
        assert!(lines.iter().any(|line| line == "execution-tool test appended --java_debug"), "{lines:?}");
        let run = fixture.run_session(cx).expect("the run's tab");
        assert!(run.is_live() && run.run_id.is_some(), "the JVM runs in the Run tool window: {run:?}");
        // The adapter's stop can arrive before the debugger's start is answered with the process.
        wait_long(cx, "the session is the process's", |cx| {
            fixture.debug_sessions(cx).first().is_some_and(|s| s.1.is_some())
        });
        assert_eq!(fixture.debug_sessions(cx), [(DebugStatus::Paused, run.run_id)]);

        run_action(cx, "debug.resume", None);
        wait_long(cx, "the session ends", |cx| {
            fixture.debug_sessions(cx).iter().all(|(status, _)| *status == DebugStatus::Terminated)
        });
        fixture.release();
        wait_long(cx, "the plugin hears the exit", |cx| fixture.heard_exit(cx).is_some());
        let heard = fixture.heard_exit(cx).expect("the exit");
        assert_eq!((heard["status"].clone(), heard["killed"].clone()), (json!(3), json!(false)));
    }

    /// An announced port that is not a TCP port (out of range, zero, not a number, past any integer)
    /// is not attached to: no session, no report.
    #[gpui_kit::test]
    async fn a_jdwp_port_out_of_range_is_not_attached_to(cx: &mut TestAppContext) {
        let (fixture, cx) = project(cx, java_detector, &[("jdwp.port", "70000\n0\nport\n99999999999999999999\n")]);
        fixture.debug_detected(cx);
        // The console line of the announcement, once shown, and the window having read past it.
        let announced = |cx: &mut VisualTestContext| {
            let session = fixture.run_session(cx)?;
            let driver = fixture.workbench.read_with(cx, |w, _| w.run_driver().cloned())?;
            driver.read_with(cx, |driver, cx| {
                let console = driver.store().read(cx).console(&session.id)?;
                console
                    .lines()
                    .iter()
                    .find(|line| line.text.ends_with("address: 99999999999999999999"))
                    .map(|line| line.id)
            })
        };
        wait_long(cx, "the announcement is read", |cx| {
            let Some(id) = announced(cx) else { return false };
            let runs = |w: &Workbench| w.plugin_contributions.java_debug.runs.values().all(|run| run.seen >= id);
            fixture.workbench.read_with(cx, |w, _| runs(w))
        });
        assert!(fixture.debug_sessions(cx).is_empty(), "nothing attached");
        assert!(fixture.attached_to().is_none());
        let titles = notification_titles(&fixture.workbench, cx);
        assert!(!titles.iter().any(|title| title.starts_with("Cannot")), "{titles:?}");
        fixture.release();
        wait_long(cx, "the plugin hears the exit", |cx| fixture.heard_exit(cx).is_some());
    }

    /// A host the JVM names in its announcement is not where the debugger goes: it attaches to the
    /// announced port on loopback only.
    #[gpui_kit::test]
    async fn an_announced_host_is_ignored_and_the_debugger_attaches_on_loopback(cx: &mut TestAppContext) {
        let (fixture, cx) = project(cx, java_detector, &[("jdwp.port", "203.0.113.9:50123")]);
        fixture.debug_detected(cx);
        wait_long(cx, "the adapter is asked to attach", |_| fixture.attached_to().is_some());
        assert_eq!(fixture.attached_to(), Some(json!({ "hostName": LOOPBACK, "port": 50123 })));
        let lines = fixture.run_lines(cx);
        assert!(lines.iter().any(|line| line.ends_with("address: 203.0.113.9:50123")), "{lines:?}");
        fixture.release();
        wait_long(cx, "the plugin hears the exit", |cx| fixture.heard_exit(cx).is_some());
    }

    /// Stop in the debugger ends the session and the process it belongs to; the plugin hears that
    /// its run was killed.
    #[gpui_kit::test]
    async fn stop_ends_the_process_and_the_session(cx: &mut TestAppContext) {
        let (fixture, cx) = project(cx, java_detector, &[("jdwp.port", JDWP_PORT)]);
        std::fs::write(fixture.root.join("main.fake"), "start\nloop\nend\n").expect("the program is written");
        fixture.debug_detected(cx);
        // The session is the process's once its start is answered; events can show it running before.
        wait_long(cx, "the attached session", |cx| {
            let process = fixture.run_session(cx).and_then(|run| run.run_id);
            fixture
                .debug_sessions(cx)
                .first()
                .is_some_and(|(status, of)| *status == DebugStatus::Running && of.is_some() && *of == process)
        });
        run_action(cx, "debug.stop", None);
        wait_long(cx, "the session ends", |cx| {
            fixture.debug_sessions(cx).first().is_some_and(|(status, _)| *status == DebugStatus::Terminated)
        });
        wait_long(cx, "the process ends", |cx| fixture.run_session(cx).is_some_and(|run| !run.is_live()));
        assert!(fixture.run_session(cx).is_some_and(|run| run.stopped), "the process was stopped");
        wait_long(cx, "the plugin hears the end", |cx| fixture.heard_exit(cx).is_some());
        let heard = fixture.heard_exit(cx).expect("the exit");
        assert_eq!(heard["killed"], json!(true), "{heard}");
    }

    /// A JVM that exits without announcing a port is only a run that ended: the Run tab shows the
    /// exit, the plugin hears its code, and no debug session is started or reported as failed.
    #[gpui_kit::test]
    async fn a_process_that_exits_before_announcing_a_port_leaves_no_debug_session(cx: &mut TestAppContext) {
        let (fixture, cx) = project(cx, java_detector, &[("release", "")]);
        fixture.debug_detected(cx);
        wait_long(cx, "the run ends", |cx| fixture.run_session(cx).is_some_and(|run| !run.is_live()));
        let run = fixture.run_session(cx).expect("the run's tab");
        assert_eq!((run.status, run.exit_code), (SessionStatus::Finished, Some(3)));
        wait_long(cx, "the plugin hears the exit", |cx| fixture.heard_exit(cx).is_some());
        assert_eq!(fixture.heard_exit(cx).map(|heard| heard["status"].clone()), Some(json!(3)));
        assert!(fixture.debug_sessions(cx).is_empty(), "nothing attached");
        assert!(fixture.attached_to().is_none());
        let titles = notification_titles(&fixture.workbench, cx);
        assert!(!titles.iter().any(|title| title.starts_with("Cannot start debugger")), "{titles:?}");
        let followed = fixture.workbench.read_with(cx, |w, _| w.plugin_contributions.java_debug.runs.is_empty());
        assert!(followed, "the ended run is no longer followed");
    }

    /// An attach the adapter refuses is reported with the adapter's message and leaves no session;
    /// the process stays in its Run tab (the user stops it there), and the plugin hears its exit.
    #[gpui_kit::test]
    async fn an_attach_the_adapter_refuses_is_reported_with_its_message(cx: &mut TestAppContext) {
        let (fixture, cx) = project(cx, java_detector, &[("jdwp.port", JDWP_PORT), ("refuse-attach", "")]);
        fixture.debug_detected(cx);
        let title = format!("Cannot start debugger for {DETECTED}");
        wait_long(cx, "the refusal is reported", |cx| notice_titled(&fixture.workbench, &title, cx).is_some());
        let notice = notice_titled(&fixture.workbench, &title, cx).expect("the report");
        assert!(notice.body.as_deref().is_some_and(|body| body.contains("Connection refused")), "{:?}", notice.body);
        assert!(fixture.debug_sessions(cx).iter().all(|(status, _)| *status == DebugStatus::Terminated));
        assert!(fixture.run_session(cx).is_some_and(|run| run.is_live()), "the JVM is left to its Run tab");
        fixture.release();
        wait_long(cx, "the plugin hears the exit", |cx| fixture.heard_exit(cx).is_some());
        assert_eq!(fixture.heard_exit(cx).map(|heard| heard["status"].clone()), Some(json!(3)));
    }

    /// A detector without the Java debugger is debugged as before, under its configuration's own
    /// adapter in the Debug tool window: nothing runs in the Run tool window and nothing attaches.
    #[gpui_kit::test]
    async fn a_plugins_debug_request_without_the_java_debugger_runs_under_its_own_adapter(cx: &mut TestAppContext) {
        let runs_main_fake = |text: &str| text.replace("args = [\"test\"]", "args = [\"main.fake\"]");
        let (fixture, cx) = project(cx, runs_main_fake, &[("release", "")]);
        fixture.debug_detected(cx);
        wait_long(cx, "the session in the Debug tool window", |cx| fixture.debug_tabs(cx) == [DETECTED]);
        wait_long(cx, "the plugin hears the end", |cx| fixture.heard_exit(cx).is_some());
        assert_eq!(fixture.heard_exit(cx).map(|heard| heard["status"].clone()), Some(json!(0)));
        assert!(fixture.run_session(cx).is_none(), "nothing ran in Run");
        assert!(fixture.attached_to().is_none(), "nothing attached");
    }
}
