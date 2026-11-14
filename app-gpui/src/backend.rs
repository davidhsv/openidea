//! The backend, in process.
//!
//! The Tauri app reaches `ide_host::ops` through IPC commands; this app links `ide-host` and
//! calls the same functions with typed arguments. The backend runs on its own Tokio runtime;
//! [`Backend::call`] awaits its work from GPUI's executors, and the events it publishes
//! ([`IdeEvent`]) are delivered on the main thread, tagged with the window they are for.

use anyhow::Context as _;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::{App, AsyncApp, Global, Task};
use ide_core::{IdeEvent, SharedSink};
use ide_host::{Host, HostConfig};
use ide_language::{LanguageRegistry, Syntax};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

/// Hosts whose deletes go to the trash folder of their test home ([`trash_into_test_home`]).
#[cfg(test)]
static TEST_TRASHES: std::sync::Mutex<Vec<std::sync::Weak<Host>>> = std::sync::Mutex::new(Vec::new());

/// Puts the folder `trash` of the test home of `dirs` in the place of the system Trash for every
/// delete of `host`, so that no test reaches the user's Trash. Each deleted path moves there under
/// a numbered name, as two deletes may remove paths of the same name.
#[cfg(test)]
fn trash_into_test_home(host: &Arc<Host>, dirs: &AppDirs) {
    use ide_core::IdeError;
    use std::sync::atomic::{AtomicU64, Ordering};
    assert!(dirs.isolated, "a test backend runs under a home of its own");
    let trash = dirs.data.parent().unwrap_or(&dirs.data).join("trash");
    let next = AtomicU64::new(1);
    host.set_trash(Some(Arc::new(move |path: &Path| {
        std::fs::create_dir_all(&trash).map_err(IdeError::from)?;
        let name = path.file_name().ok_or_else(|| IdeError::invalid("a path without a name"))?;
        let number = next.fetch_add(1, Ordering::Relaxed);
        std::fs::rename(path, trash.join(format!("{number}-{}", name.to_string_lossy()))).map_err(IdeError::from)
    })));
    let mut trashes = TEST_TRASHES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    trashes.retain(|trashed| trashed.strong_count() > 0);
    trashes.push(Arc::downgrade(host));
}

/// The projects a test's backend opened, closed when the test's thread ends. A test app is never
/// shut down (its windows stay, and with them their projects), so without this every project a
/// test opened keeps its file watchers and workers until the test binary exits, and the process
/// runs out of the FSEvents streams macOS allows it.
#[cfg(test)]
struct CloseWhenTheTestEnds {
    host: Arc<Host>,
    windows: Arc<parking_lot::Mutex<std::collections::BTreeSet<String>>>,
}

#[cfg(test)]
impl Drop for CloseWhenTheTestEnds {
    fn drop(&mut self) {
        for window in std::mem::take(&mut *self.windows.lock()) {
            self.host.close_project_detached(&window);
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PROJECTS: std::cell::RefCell<Vec<CloseWhenTheTestEnds>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Deletes `path` as File › Delete does ([`ide_host::ops::fs::delete`]): Local History keeps its
/// contents and it moves to the system Trash, or where the host's override says. In a test build a
/// host without the test trash of [`Backend::start`] panics instead of reaching the system Trash.
pub async fn delete_path(host: &Host, window: &str, path: PathBuf) -> ide_core::IdeResult<()> {
    #[cfg(test)]
    assert!(
        TEST_TRASHES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|trashed| trashed.upgrade().is_some_and(|trashed| std::ptr::eq(Arc::as_ptr(&trashed), host))),
        "a test deletes {} with a host whose deletes reach the system Trash",
        path.display()
    );
    ide_host::ops::fs::delete(host, window, path).await
}

/// Identifier of the app's directories. Its own, so the GPUI app never shares settings, caches
/// or single-instance state with an installed Tauri build running next to it.
pub const APP_IDENTIFIER: &str =
    if cfg!(debug_assertions) { "ai.l0ng.openidea.gpui.dev" } else { "ai.l0ng.openidea.gpui" };

/// Overrides every app directory with subfolders of one folder (tests, measurements).
pub const HOME_ENV: &str = "OPENIDEA_GPUI_HOME";

/// Where the app keeps its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    pub config: PathBuf,
    /// Persistent user state, including credentials and recovery history.
    pub data: PathBuf,
    /// Machine-local tools and rebuildable caches.
    pub local_data: PathBuf,
    /// Under a home of its own (tests, [`HOME_ENV`]): saved passwords stay in files there, never
    /// in the user's keychain.
    pub isolated: bool,
}

impl AppDirs {
    /// The platform's directories for [`APP_IDENTIFIER`], or the folder [`HOME_ENV`] names.
    pub fn resolve() -> anyhow::Result<Self> {
        if let Some(home) = std::env::var_os(HOME_ENV).filter(|h| !h.is_empty()) {
            return Ok(Self::under(Path::new(&home)));
        }
        let dir = |base: Option<PathBuf>, what: &str| {
            base.map(|b| b.join(APP_IDENTIFIER)).with_context(|| format!("no {what} directory on this system"))
        };
        Ok(Self {
            config: dir(dirs::config_dir(), "configuration")?,
            data: dir(dirs::data_dir(), "data")?,
            local_data: dir(dirs::data_local_dir(), "local data")?,
            isolated: false,
        })
    }

    /// `config`, `data` and `local-data` inside `home`.
    pub fn under(home: &Path) -> Self {
        Self {
            config: home.join("config"),
            data: home.join("data"),
            local_data: home.join("local-data"),
            isolated: true,
        }
    }
}

/// An event from the backend: for one window (by label), or for every window.
#[derive(Debug, Clone)]
pub struct BackendEvent {
    pub window: Option<String>,
    pub event: IdeEvent,
}

type Listener = dyn Fn(&BackendEvent, &mut App);

/// The process-wide backend: the [`Host`] plus what delivers its work and events to the UI.
pub struct Backend {
    pub host: Arc<Host>,
    pub dirs: AppDirs,
    runtime: tokio::runtime::Runtime,
    listeners: Vec<(u64, Rc<Listener>)>,
    next_listener: u64,
    events: UnboundedSender<BackendEvent>,
    /// Kept so the event pump lives as long as the backend.
    _pump: Task<()>,
    /// The saved Git HTTPS credentials of this backend's data directory.
    git_credentials: Option<ide_ssh::git_creds::GitCredentials>,
}

impl Global for Backend {}

struct ChannelSink {
    window: Option<String>,
    events: UnboundedSender<BackendEvent>,
}

impl ide_core::EventSink for ChannelSink {
    fn emit(&self, event: IdeEvent) {
        // The receiver is gone only while the app shuts down.
        let _ = self.events.unbounded_send(BackendEvent { window: self.window.clone(), event });
    }
}

impl Backend {
    /// Starts the backend with the app's directories and installs it as a global.
    pub fn init(dirs: AppDirs, cx: &mut App) -> anyhow::Result<()> {
        let (backend, events) = Self::start(dirs, cx)?;
        cx.set_global(backend);
        Self::pump(events, cx);
        Ok(())
    }

    fn start(dirs: AppDirs, cx: &mut App) -> anyhow::Result<(Self, UnboundedReceiver<BackendEvent>)> {
        ide_workspace::process::mark_start();
        std::fs::create_dir_all(&dirs.config).with_context(|| format!("cannot create {}", dirs.config.display()))?;
        std::fs::create_dir_all(&dirs.data).with_context(|| format!("cannot create {}", dirs.data.display()))?;
        if let Err(error) = std::fs::create_dir_all(&dirs.local_data) {
            tracing::warn!(local_data_dir = ?dirs.local_data, %error, "cannot create local data directory");
        }
        tracing::info!(data_dir = ?dirs.data, local_data_dir = ?dirs.local_data, "application data directories");
        // Before the first saved-password store exists (git credentials here, then the host's).
        if dirs.isolated {
            ide_ssh::secrets::keep_secrets_in_files();
        }
        // Saved git HTTPS credentials (askpass answers them silently). The store installed is the
        // process's; this backend keeps the one it installed, so a backend started meanwhile
        // elsewhere in the process (tests) does not hand it another directory's.
        let git_credentials = {
            static INSTALL: parking_lot::Mutex<()> = parking_lot::const_mutex(());
            let _installing = INSTALL.lock();
            ide_ssh::git_creds::init(dirs.data.join("git-credentials.json"));
            ide_ssh::git_creds::get()
        };
        // File › Invalidate Caches… requested at the last run.
        let removed = ide_workspace::caches::apply_pending(&dirs.data, &dirs.local_data);
        if !removed.is_empty() {
            tracing::info!("invalidated caches: {removed:?}");
        }
        ide_host::data_dirs::migrate(&dirs.data, &dirs.local_data);

        // Grammar set-up is not needed for the first paint: build it off the main thread.
        let syntax: Arc<OnceLock<Arc<Syntax>>> = Arc::default();
        {
            let syntax = syntax.clone();
            std::thread::Builder::new().name("openidea-syntax-init".into()).spawn(move || {
                syntax.get_or_init(|| Arc::new(Syntax::new()));
            })?;
        }
        let mut runtime = tokio::runtime::Builder::new_multi_thread();
        // A test app keeps its backend until the test binary exits, and a process may run only so many
        // threads: the tests' runtimes keep two workers each instead of one per core.
        #[cfg(test)]
        runtime.worker_threads(2);
        let runtime = runtime.enable_all().thread_name("openidea-backend").build()?;
        let languages = Arc::new(
            LanguageRegistry::load(Some(&dirs.config.join("languages"))).map_err(|e| anyhow::anyhow!(e.message))?,
        );
        let (sender, events) = unbounded();
        #[cfg(test)]
        let windows: Arc<parking_lot::Mutex<std::collections::BTreeSet<String>>> = Arc::default();
        let host = Arc::new(Host::new(HostConfig {
            config_dir: dirs.config.clone(),
            data_dir: dirs.data.clone(),
            local_data_dir: dirs.local_data.clone(),
            languages,
            syntax,
            sink_for: Box::new({
                let sender = sender.clone();
                #[cfg(test)]
                let windows = windows.clone();
                move |window| -> SharedSink {
                    #[cfg(test)]
                    windows.lock().insert(window.to_owned());
                    Arc::new(ChannelSink { window: Some(window.to_owned()), events: sender.clone() })
                }
            }),
            owner: owner_window,
            runtime: runtime.handle().clone(),
            project_environment: false,
        }));
        #[cfg(test)]
        trash_into_test_home(&host, &dirs);
        #[cfg(test)]
        TEST_PROJECTS.with(|projects| projects.borrow_mut().push(CloseWhenTheTestEnds { host: host.clone(), windows }));
        let backend = Self {
            host,
            dirs,
            runtime,
            listeners: Vec::new(),
            next_listener: 0,
            events: sender,
            _pump: Task::ready(()),
            git_credentials,
        };
        let _ = cx;
        Ok((backend, events))
    }

    /// Delivers backend events to the listeners, on the main thread, in order.
    fn pump(mut events: UnboundedReceiver<BackendEvent>, cx: &mut App) {
        let pump = cx.spawn(async move |cx: &mut AsyncApp| {
            while let Some(event) = events.next().await {
                cx.update(|cx| {
                    let listeners: Vec<Rc<Listener>> =
                        cx.global::<Backend>().listeners.iter().map(|(_, l)| l.clone()).collect();
                    for listener in listeners {
                        listener(&event, cx);
                    }
                });
            }
        });
        cx.global_mut::<Backend>()._pump = pump;
    }

    /// Announces `event` from the UI side to `window`, or to every window: it reaches the
    /// listeners in order with the backend's own events.
    pub fn emit(&self, window: Option<&str>, event: IdeEvent) {
        // The receiver is gone only while the app shuts down.
        let _ = self.events.unbounded_send(BackendEvent { window: window.map(str::to_owned), event });
    }

    /// A fresh id for a cancellable backend request, unique in this process.
    pub fn next_request_id() -> ide_core::RequestId {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        ide_core::RequestId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as _)
    }

    /// Calls `listener` for every backend event; returns its id for [`Backend::unlisten`].
    pub fn listen(&mut self, listener: impl Fn(&BackendEvent, &mut App) + 'static) -> u64 {
        self.next_listener += 1;
        self.listeners.push((self.next_listener, Rc::new(listener)));
        self.next_listener
    }

    pub fn unlisten(&mut self, id: u64) {
        self.listeners.retain(|(listener, _)| *listener != id);
    }

    /// The saved Git HTTPS credentials (Settings › Git › Passwords), when their store started.
    pub fn git_credentials(&self) -> Option<&ide_ssh::git_creds::GitCredentials> {
        self.git_credentials.as_ref()
    }

    /// Runs backend work on the backend's runtime; the returned future is awaited from GPUI's
    /// executors. Dropping it does not cancel the work (cancellable operations take a request id).
    pub fn call<T, W>(&self, work: W) -> impl Future<Output = anyhow::Result<T>> + Send + 'static + use<T, W>
    where
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
    {
        let handle = self.runtime.spawn(work);
        async move { handle.await.context("the backend task did not complete") }
    }

    /// Runs backend work that takes the host.
    pub fn with_host<T, F, W>(&self, work: W) -> impl Future<Output = anyhow::Result<T>> + Send + 'static + use<T, F, W>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
        W: FnOnce(Arc<Host>) -> F + Send + 'static,
    {
        let host = self.host.clone();
        self.call(async move { work(host).await })
    }

    /// Blocks until `work` finishes on the backend's runtime (start-up and tests only).
    pub fn block_on<T>(&self, work: impl Future<Output = T>) -> T {
        self.runtime.block_on(work)
    }
}

/// The window that owns what `window` starts: a settings or detached-editor window's parent,
/// else `window`. Auxiliary windows are labelled `<parent>::<kind>`.
pub fn owner_window(window: &str) -> &str {
    window.split_once("::").map_or(window, |(parent, _)| parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures").join(name).canonicalize().expect("the fixture exists")
    }

    #[test]
    fn auxiliary_windows_belong_to_their_parent() {
        assert_eq!(owner_window("main"), "main");
        assert_eq!(owner_window("w3::settings"), "w3");
    }

    #[test]
    fn directories_follow_the_home_override() {
        let dirs = AppDirs::under(Path::new("/tmp/x"));
        assert_eq!(dirs.config, Path::new("/tmp/x/config"));
        assert_eq!(dirs.local_data, Path::new("/tmp/x/local-data"));
        assert!(dirs.isolated, "a home of its own keeps its saved passwords out of the keychain");
    }

    /// The SSH password the Settings dialog applies and the database password a data source saves
    /// go through the same backend calls as the UI's, and land in owner-only files under the
    /// temporary home; the OS credential store is never set up in this process.
    #[gpui_kit::test]
    async fn saved_passwords_stay_in_files_under_a_temporary_home(cx: &mut TestAppContext) {
        const SSH_SECRET: &str = "not-a-real-ssh-password";
        const DB_SECRET: &str = "not-a-real-database-password";
        cx.executor().allow_parking();
        let home = tempfile::tempdir().expect("a temporary home");
        let project = tempfile::tempdir().expect("a temporary project");
        std::fs::write(project.path().join("README.md"), "# p\n").expect("a project file");
        let root = project.path().canonicalize().expect("the project root");
        cx.update(|cx| Backend::init(AppDirs::under(home.path()), cx)).expect("the backend starts");
        assert!(ide_ssh::secrets::secrets_in_files(), "the isolated home keeps saved passwords in files");

        let saved = cx.update(|cx| {
            let root = root.clone();
            cx.global::<Backend>().with_host(move |host| async move {
                ide_host::ops::project::open(&host, "w1", &root).await?;
                host.ssh.set_secret("s1", Some(SSH_SECRET.to_owned()), true);
                ide_host::ops::db::set_password(&host, "w1", "pg", Some(DB_SECRET.to_owned()), true).await
            })
        });
        saved.await.expect("the task completes").expect("the passwords are saved");

        let data = home.path().join("data");
        let config =
            ide_core::remote_settings::SshConfiguration { id: "s1".into(), save_secret: true, ..Default::default() };
        let ssh = ide_ssh::SshManager::new(data.join("ssh-passwords.json"));
        assert_eq!(ssh.secret(&config).as_deref(), Some(SSH_SECRET), "read back from the file");
        let db = ide_db::secrets::SecretStore::new(data.join("db-passwords.json"), &root);
        assert_eq!(db.get("pg").as_deref(), Some(DB_SECRET), "read back from the file");
        for (file, secret) in [("ssh-passwords.json", SSH_SECRET), ("db-passwords.json", DB_SECRET)] {
            let vault = data.join(format!("{file}.vault"));
            let text = std::fs::read_to_string(&vault).expect("the vault file");
            assert!(text.contains(secret), "{file}.vault holds the secret");
            assert!(!std::fs::read_to_string(data.join(file)).expect("the index").contains(secret));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&vault).expect("the vault file").permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{file}.vault is owner-only");
            }
        }

        assert!(!ide_ssh::secrets::default_store_installed(), "the OS credential store was set up");
        cx.update(|cx| cx.global::<Backend>().host.close_project("w1"));
    }

    /// The real backend opens a fixture project and lists it through `ide_host::ops`, and its
    /// events arrive on the main thread tagged with the window.
    #[gpui_kit::test]
    async fn opens_a_project_and_lists_its_files(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let home = tempfile::tempdir().expect("a temporary home");
        cx.update(|cx| Backend::init(AppDirs::under(home.path()), cx)).expect("the backend starts");
        let seen: Rc<std::cell::RefCell<Vec<Option<String>>>> = Rc::default();
        let log = seen.clone();
        cx.update(|cx| cx.global_mut::<Backend>().listen(move |event, _| log.borrow_mut().push(event.window.clone())));

        let root = fixture("rust-app");
        let open = cx.update(|cx| {
            let root = root.clone();
            cx.global::<Backend>().with_host(move |host| async move {
                let project = ide_host::ops::project::open(&host, "w1", &root).await?;
                let info = ide_host::ops::project::info(&project);
                let entries = ide_host::ops::fs::read_dir(&host, "w1", root).await?;
                ide_core::IdeResult::Ok((info.name, entries))
            })
        });
        let (name, entries) = open.await.expect("the task completes").expect("the project opens");
        assert_eq!(name, "rust-app");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"Cargo.toml") && names.contains(&"src"), "{names:?}");

        // A document reports its unsaved state to the window that owns it.
        let host = cx.update(|cx| cx.global::<Backend>().host.clone());
        let documents = host.project("w1").expect("the project is open").documents.clone();
        let info = documents.open(&root.join("Cargo.toml")).expect("the file opens");
        documents.replace_all(info.id, "# changed\n").expect("the edit applies");
        for _ in 0..200 {
            cx.run_until_parked();
            if !seen.borrow().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(seen.borrow().iter().any(|w| w.as_deref() == Some("w1")), "events seen: {:?}", seen.borrow());
        assert!(seen.borrow().iter().all(|w| w.as_deref() == Some("w1") || w.is_none()));
        host.close_project("w1");
    }
}
