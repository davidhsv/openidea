//! The Pull Requests tool window of a project: the GitHub connection, the list's requests, one
//! details tab per opened pull request, the review flow's plain mutations and Checkout, carried out
//! through gh (`ide_github`) off the window's thread.
//!
//! The window owns its tabs and the list; this slot owns the connection ([`RepositoryState`]), the
//! repository's cached lists ([`RepositoryData`]), the running review flow actions ([`FlowBusy`])
//! and each `#N` tab's [`DetailsPanel`] with what was loaded for it. A connect goes: the roots'
//! remotes → gh's availability in the repository likely to be connected → the repository and the
//! signed-in user → the window shows the list. gh runs in the connected local repository.
//!
//! gh keeps the account's credentials itself; nothing here reads, stores or shows them. A remote
//! URL that carries a credential is masked before it is kept, and gh's and git's output is masked
//! the same way before it reaches a notification or the log.

use super::branches::{This, finished, git_command_ended, git_command_starts, refresh, run_action_later, save_all};
use super::services_window::on_workbench;
use super::{Panel, Workbench};
use crate::backend::Backend;
use crate::github::api::{graphql_data, parse_rest, repo_path, repo_variables, rest_request};
use crate::github::batches::{LoadStep, PageOutcome, PageTicket};
use crate::github::data_model::build_mergeability_state;
use crate::github::details::{is_head_checked_out, resolve_conflicts};
use crate::github::details_panel::{DetailsEvent, DetailsInputs, DetailsPanel};
use crate::github::details_view::{FlowBusy, FlowButton};
use crate::github::list::{Chip, ListRequest};
use crate::github::list_panel::{ListPanelEvent, label_items, user_items};
use crate::github::new_comments::document_request;
use crate::github::repository::{
    AvailabilityRequest, ConnectOutcome, ConnectRequest, Disconnected, RefreshStep, RefreshTicket, RepositoryState,
    RootRemotes, selection_key,
};
use crate::github::repository_data::{
    RepositoryData, RestList, UsersSource, parse_collaborators_page, parse_labels_page, parse_users_page,
    rest_page_request, users_source,
};
use crate::github::review_diff::PULL_REQUEST_DIFF;
use crate::github::toolwindow::{ChangeRepository, GhTab, change_repository};
use crate::github::types::{
    ChangedFile, CommitFile, Connection, Coordinates, FileViewedState, GhCommitShort, GhPullRequest, GhRepository,
    GhReviewer, GhUser, MergeabilityData, MergeabilityState, Permission,
};
use crate::github::window_view::{PULL_REQUESTS_TOOL_WINDOW, PullRequestsEvent, PullRequestsWindow};
use crate::platform::actions::ActionRegistry;
use crate::platform::context::set_context;
use crate::platform::i18n::{Localization, Param};
use crate::platform::storage::UiStorage;
use crate::vcs::git_forms::redacted_url;
use crate::vcs_diff::diff_input::{DiffData, DiffSide};
use crate::workbench::notifications::{NotificationLevel, NotificationRequest};
use gpui_kit::{App, AsyncWindowContext, ClipboardItem, Context, Entity, Subscription, Task, Window, prelude::*};
use ide_core::{CancellationToken, IdeResult};
use ide_github::{GhApiRequest, GhAvailability, GhCli, PrDiffRevisions};
use ide_host::Host;
use ide_host::ops::vcs as git;
use ide_vcs::Remote;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The action that runs a program in a new Terminal tab (`{ program, args, name?, cwd? }`).
const TERMINAL_RUN_COMMAND: &str = "terminal.runCommand";

/// What stands in for gh in a test: the program the window runs instead of the machine's gh, how
/// many gh calls the window started, and the commands it asked to run in a terminal (recorded,
/// never started). Without one, a test build runs no gh at all.
#[cfg(test)]
pub(super) struct GhStandIn {
    pub(super) cli: GhCli,
    pub(super) started: std::cell::Cell<usize>,
    pub(super) terminal: std::cell::RefCell<Vec<Value>>,
}

#[cfg(test)]
impl gpui_kit::Global for GhStandIn {}

/// The Pull Requests window of the open project and what it loaded.
pub(super) struct GithubSlot {
    /// The project root: the storage keys of the saved repository choice and of the searches.
    root: String,
    /// The project's primary repository, where gh runs while no repository is connected.
    primary: Option<String>,
    repo: RepositoryState,
    data: RepositoryData,
    busy: FlowBusy,
    window: Entity<PullRequestsWindow>,
    /// The `#N` tabs, by number.
    tabs: HashMap<u32, PullRequestTab>,
    /// Whether the tool window was last known to be shown.
    shown: bool,
    /// Refresh List asked for the list again once the connection is checked.
    reload_on_connect: bool,
    refreshing: Option<Task<()>>,
    connecting: Option<Task<()>>,
    /// The list page on its way (a newer search makes the older answer void).
    list_fetch: Option<Task<()>>,
    _events: Subscription,
}

/// Changed files as gh answered them, or why they did not load.
type LoadedFiles = Result<Vec<CommitFile>, String>;

/// A `#N` tab: its panel and what was loaded for it.
struct PullRequestTab {
    panel: Entity<DetailsPanel>,
    details: Option<GhPullRequest>,
    mergeability: Option<MergeabilityState>,
    commits: Vec<GhCommitShort>,
    /// The pull request's changes; `None` while they load.
    files: Option<LoadedFiles>,
    viewed: HashMap<String, FileViewedState>,
    /// The commit chosen in the picker and its changes (`None` while they load).
    commit: Option<(String, Option<LoadedFiles>)>,
    /// Bumped by each load: the answers of an older one are dropped.
    generation: u64,
    loading: Option<Task<()>>,
    commit_loading: Option<Task<()>>,
    _events: Subscription,
}

/// A REST user (`GET user`) as the GraphQL `user` shape.
#[derive(Deserialize)]
struct RestUser {
    login: String,
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    avatar_url: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    name: Option<String>,
}

impl From<RestUser> for GhUser {
    fn from(user: RestUser) -> Self {
        GhUser {
            typename: "User".to_owned(),
            id: user.node_id,
            login: user.login,
            url: user.html_url,
            avatar_url: user.avatar_url,
            name: user.name,
        }
    }
}

/// A `pullRequestCommits` node.
#[derive(Deserialize)]
struct CommitNode {
    #[serde(default)]
    commit: GhCommitShort,
}

/// The list a chooser of the list's search loads its items from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChooserSource {
    Collaborators,
    Contributors,
    Assignees,
    Labels,
}

impl ChooserSource {
    /// Authors come from the collaborators for users who can write, else from the contributors.
    fn of(chip: Chip, permission: Option<Permission>) -> Option<Self> {
        match chip {
            Chip::Author => Some(match users_source(permission) {
                UsersSource::Collaborators => Self::Collaborators,
                UsersSource::Contributors => Self::Contributors,
            }),
            Chip::Assignee => Some(Self::Assignees),
            Chip::Label => Some(Self::Labels),
            Chip::State | Chip::Review | Chip::Sort => None,
        }
    }

    fn list(self) -> RestList {
        match self {
            Self::Collaborators => RestList::Collaborators,
            Self::Contributors => RestList::Contributors,
            Self::Assignees => RestList::Assignees,
            Self::Labels => RestList::Labels,
        }
    }

    fn load(self, data: &mut RepositoryData) -> LoadStep<u32> {
        match self {
            Self::Collaborators => data.collaborators.load(),
            Self::Contributors => data.contributors.load(),
            Self::Assignees => data.assignees.load(),
            Self::Labels => data.labels.load(),
        }
    }

    fn page_loaded(
        self,
        data: &mut RepositoryData,
        ticket: &PageTicket<u32>,
        answer: Result<String, String>,
    ) -> PageOutcome<u32> {
        let page = ticket.cursor;
        match self {
            Self::Collaborators => {
                data.collaborators.page_loaded(ticket, answer.and_then(|text| parse_collaborators_page(page, &text)))
            }
            Self::Contributors => {
                data.contributors.page_loaded(ticket, answer.and_then(|text| parse_users_page(page, &text)))
            }
            Self::Assignees => {
                data.assignees.page_loaded(ticket, answer.and_then(|text| parse_users_page(page, &text)))
            }
            Self::Labels => data.labels.page_loaded(ticket, answer.and_then(|text| parse_labels_page(page, &text))),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// gh, off the window's thread

/// The gh to run: `None` is the machine's own, looked up off the window's thread.
#[cfg(not(test))]
fn gh_program(_: &App) -> Result<Option<GhCli>, String> {
    Ok(None)
}

/// The gh to run in a test: its stand-in, counted; without one gh is not run at all, so a test never
/// reaches the machine's gh or its account.
#[cfg(test)]
fn gh_program(cx: &App) -> Result<Option<GhCli>, String> {
    let stand_in =
        cx.try_global::<GhStandIn>().ok_or_else(|| "gh does not run in tests without a stand-in".to_owned())?;
    stand_in.started.set(stand_in.started.get() + 1);
    Ok(Some(stand_in.cli.clone()))
}

/// The machine's gh ([`GhCli::located`]).
#[cfg(not(test))]
fn located_gh() -> GhCli {
    GhCli::located()
}

/// A test never runs the machine's gh, which would reach GitHub with the account signed in there.
#[cfg(test)]
fn located_gh() -> GhCli {
    panic!("a test asked for the machine's gh; it runs only the stand-in it sets")
}

/// Runs blocking gh `work` for window `label` on the backend's runtime with the gh to run; the
/// failure is its message. `command` is the backend command the work stands for: like every command
/// that runs a program, it is refused (and nothing runs) while the window's project is in Restricted
/// Mode.
fn gh_call<T, W>(
    cx: &App,
    label: &str,
    command: &str,
    work: W,
) -> impl Future<Output = Result<T, String>> + 'static + use<T, W>
where
    T: Send + 'static,
    W: FnOnce(&GhCli) -> Result<T, String> + Send + 'static,
{
    let authorized = cx.try_global::<Backend>().map(|backend| backend.host.authorize_command(label, command));
    let program = match authorized {
        Some(Err(error)) => Err(error.message),
        _ => gh_program(cx),
    };
    #[cfg(test)]
    assert!(!matches!(program, Ok(None)), "a test asked for the machine's gh; it runs only the stand-in it sets");
    let call = cx.try_global::<Backend>().map(|backend| {
        backend.call(async move {
            tokio::task::spawn_blocking(move || {
                // Looking gh up reads the disk, so it happens here and not on the window's thread.
                let cli = program?.unwrap_or_else(located_gh);
                work(&cli)
            })
            .await
        })
    });
    async move {
        match call {
            None => Err("The backend is not running".to_owned()),
            Some(call) => match call.await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(format!("the gh call did not complete: {error}")),
                Err(error) => Err(error.to_string()),
            },
        }
    }
}

/// Runs `program` with `args` in a new Terminal tab, as Run Command in Terminal does.
#[cfg(not(test))]
fn run_in_terminal(arg: Option<Value>, window: &mut Window, cx: &mut App) {
    run_action_later(TERMINAL_RUN_COMMAND.to_owned(), arg, window, cx);
}

/// In a test the stand-in for gh records the command: no terminal starts and no gh runs.
#[cfg(test)]
fn run_in_terminal(arg: Option<Value>, _: &mut Window, cx: &mut App) {
    match cx.try_global::<GhStandIn>() {
        Some(stand_in) => stand_in.terminal.borrow_mut().push(arg.unwrap_or_default()),
        None => tracing::warn!("{TERMINAL_RUN_COMMAND} is not run in a test without a stand-in for gh"),
    }
}

/// `text` with the credentials of every URL in it masked, for a notification or the log: each word
/// (between spaces, quotes and brackets) is masked as a URL on its own.
fn redacted_text(text: &str) -> String {
    let delimiter = |c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '<' | '>' | '(' | ')' | '[' | ']');
    text.split_inclusive(delimiter)
        .map(|piece| {
            let word = piece.trim_end_matches(delimiter);
            format!("{}{}", redacted_url(word), &piece[word.len()..])
        })
        .collect()
}

/// `remotes` as they may be kept and shown: a credential in a URL is masked. Owner and name stay,
/// which is all the window reads from them (gh signs in with its own account).
fn without_credentials(remotes: Vec<Remote>) -> Vec<Remote> {
    remotes.into_iter().map(|remote| Remote { url: redacted_url(&remote.url).into_owned(), ..remote }).collect()
}

/// `gh api` in `dir`: the response body, or gh's message.
fn gh_api(cli: &GhCli, dir: &Path, request: &GhApiRequest) -> Result<String, String> {
    cli.api(dir, request).map_err(|error| error.message)
}

fn unexpected<E: std::fmt::Display>(error: E) -> String {
    format!("unexpected GitHub answer: {error}")
}

/// A pull request document of `repo` (`$repoOwner`, `$repoName`, `$number`, and `$cursor` when paged).
fn pull_request_request(
    query: &str,
    repo: &Coordinates,
    number: u32,
    cursor: Option<&str>,
) -> Result<GhApiRequest, String> {
    let [owner, name] = repo_variables(repo);
    document_request(query, &[owner, name, ("number", Some(Value::from(number))), ("cursor", cursor.map(Value::from))])
}

/// `repository.pullRequest` of an answer.
fn pull_request_node(text: &str) -> Result<Value, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repository {
        pull_request: Option<Value>,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Option<Repository>,
    }
    let data = graphql_data::<Data>(text).map_err(|error| error.message)?;
    data.repository.and_then(|r| r.pull_request).ok_or_else(|| "The pull request was not found".to_owned())
}

/// Every page of the pull request's connection `field` (`commits`, `files`).
fn pull_request_pages<T: DeserializeOwned>(
    cli: &GhCli,
    dir: &Path,
    pull_request: (&Coordinates, u32),
    query: &str,
    field: &str,
) -> Result<Vec<T>, String> {
    let (repo, number) = pull_request;
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let request = pull_request_request(query, repo, number, cursor.as_deref())?;
        let node = pull_request_node(&gh_api(cli, dir, &request)?)?;
        let Connection { page_info, nodes }: Connection<T> =
            serde_json::from_value(node.get(field).cloned().unwrap_or(Value::Null)).map_err(unexpected)?;
        items.extend(nodes);
        match page_info.end_cursor {
            Some(next) if page_info.has_next_page => cursor = Some(next),
            _ => return Ok(items),
        }
    }
}

/// The repository (`None`: not found) and the signed-in user.
fn find_repository_and_user(
    cli: &GhCli,
    dir: &Path,
    repo: &Coordinates,
) -> Result<(Option<GhRepository>, GhUser), String> {
    #[derive(Deserialize)]
    struct Data {
        repository: Option<GhRepository>,
    }
    let request = document_request("findRepository", &repo_variables(repo))?;
    let repository = graphql_data::<Data>(&gh_api(cli, dir, &request)?).map_err(|error| error.message)?.repository;
    let text = gh_api(cli, dir, &rest_request("GET", "user", None, false))?;
    let user: RestUser = serde_json::from_str(&text).map_err(unexpected)?;
    Ok((repository, user.into()))
}

fn find_pull_request(cli: &GhCli, dir: &Path, repo: &Coordinates, number: u32) -> Result<GhPullRequest, String> {
    let request = pull_request_request("findPullRequest", repo, number, None)?;
    serde_json::from_value(pull_request_node(&gh_api(cli, dir, &request)?)?).map_err(unexpected)
}

fn find_mergeability(cli: &GhCli, dir: &Path, repo: &Coordinates, number: u32) -> Result<MergeabilityData, String> {
    let request = pull_request_request("findPullRequestMergeability", repo, number, None)?;
    serde_json::from_value(pull_request_node(&gh_api(cli, dir, &request)?)?).map_err(unexpected)
}

/// The changes of a REST answer: an array, or (`commits/{sha}`) an object with `files`.
fn rest_files(text: &str, paginated: bool) -> Result<Vec<CommitFile>, String> {
    #[derive(Deserialize)]
    struct WithFiles {
        #[serde(default)]
        files: Vec<CommitFile>,
    }
    match parse_rest(text, paginated).map_err(unexpected)? {
        None => Ok(Vec::new()),
        Some(Value::Array(files)) => serde_json::from_value(Value::Array(files)).map_err(unexpected),
        Some(other) => serde_json::from_value::<WithFiles>(other).map(|w| w.files).map_err(unexpected),
    }
}

fn pull_request_files(cli: &GhCli, dir: &Path, repo: &Coordinates, number: u32) -> Result<Vec<CommitFile>, String> {
    let endpoint = format!("{}/pulls/{number}/files?per_page=100", repo_path(repo));
    rest_files(&gh_api(cli, dir, &rest_request("GET", &endpoint, None, true))?, true)
}

fn commit_files(cli: &GhCli, dir: &Path, repo: &Coordinates, oid: &str) -> Result<Vec<CommitFile>, String> {
    let endpoint = format!("{}/commits/{oid}", repo_path(repo));
    rest_files(&gh_api(cli, dir, &rest_request("GET", &endpoint, None, false))?, false)
}

/// The viewed state of each changed file, by path.
fn viewed_states(
    cli: &GhCli,
    dir: &Path,
    repo: &Coordinates,
    number: u32,
) -> Result<HashMap<String, FileViewedState>, String> {
    let files: Vec<ChangedFile> = pull_request_pages(cli, dir, (repo, number), "pullRequestFiles", "files")?;
    Ok(files.into_iter().map(|f| (f.path, f.viewer_viewed_state)).collect())
}

/// Every local repository of the project with its remotes (credentials masked), primary first, and
/// the primary's root.
async fn list_remotes(host: Arc<Host>, label: String) -> IdeResult<(Vec<RootRemotes>, Option<String>)> {
    let roots = git::vcs_roots(&host, &label)?;
    if roots.is_empty() {
        // Not a repository: gh is asked in the project's folder, which has no remotes to offer.
        let remotes = git::vcs_remotes(&host, &label, None).await.unwrap_or_default();
        return Ok((vec![RootRemotes { root: None, remotes: without_credentials(remotes) }], None));
    }
    let primary = roots.first().map(|root| root.root.0.to_string_lossy().into_owned());
    let mut lists = Vec::with_capacity(roots.len());
    for root in roots {
        let path = root.root.0.clone();
        // A repository whose remotes cannot be read offers none.
        let remotes = git::vcs_remotes(&host, &label, Some(path.clone())).await.unwrap_or_default();
        let root = Some(path.to_string_lossy().into_owned());
        lists.push(RootRemotes { root, remotes: without_credentials(remotes) });
    }
    Ok((lists, primary))
}

/// Where gh runs for `root` (`None`: the primary repository, else the project's folder).
fn gh_dir(root: Option<&str>, slot: &GithubSlot) -> PathBuf {
    PathBuf::from(root.or(slot.primary.as_deref()).unwrap_or(&slot.root))
}

/// The diff of `path` in the pull request (`commit`: in that commit only), between revisions
/// `prepare_diff` made available.
fn pull_request_diff(number: u32, path: &str, commit: Option<&str>, revisions: &PrDiffRevisions) -> DiffData {
    let root = &revisions.root.0;
    let absolute = |relative: &str| root.join(relative).to_string_lossy().into_owned();
    let (left, right) = match commit {
        Some(oid) => (format!("{oid}^"), oid.to_owned()),
        None => (revisions.base.clone(), revisions.head.clone()),
    };
    let old_path = revisions.files.iter().find(|f| f.path == path).and_then(|f| f.old_path.as_deref()).map(absolute);
    let mut data = DiffData::new(
        &absolute(path),
        DiffSide::Rev { rev: left, path: old_path, label: None },
        DiffSide::Rev { rev: right, path: None, label: None },
    );
    let mut context = Map::new();
    context.insert("kind".to_owned(), Value::from(PULL_REQUEST_DIFF));
    context.insert("number".to_owned(), Value::from(number));
    context.insert("path".to_owned(), Value::from(path));
    if let Some(oid) = commit {
        context.insert("commit".to_owned(), Value::from(oid));
    }
    data.context = Some(context);
    data
}

/// The pull request number an action is given.
fn number_arg(arg: Option<&Value>) -> Option<u32> {
    arg.and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}

/// The `requested_reviewers` body naming `reviewers`: user logins and team slugs.
fn reviewers_body(reviewers: &[GhReviewer]) -> Value {
    let mut logins = Vec::new();
    let mut teams = Vec::new();
    for reviewer in reviewers {
        match reviewer {
            GhReviewer::Team(team) => teams.push(team.slug.clone()),
            GhReviewer::Actor(actor) => logins.push(actor.login.clone()),
        }
    }
    json!({ "reviewers": logins, "team_reviewers": teams })
}

/// A GitHub notice of the Git group under `key`.
fn github_notice(level: NotificationLevel, title: String, key: &str) -> NotificationRequest {
    let mut request = NotificationRequest::new(level, title);
    request.group = Some("vcs".to_owned());
    request.key = Some(key.to_owned());
    request
}

fn failure(title: &str, body: &str, cx: &mut App) -> NotificationRequest {
    let title = cx.default_global::<Localization>().0.tx(title);
    let mut request = github_notice(NotificationLevel::Error, title, "github-op");
    request.body = Some(redacted_text(body));
    request
}

/// Fetches pull request `number`'s branch with gh and checks it out as a Git operation of the window
/// (no other one starts and no read of the repository runs meanwhile), then reads the repository
/// again so the branch widget and the open tabs follow, and says how it went.
async fn run_checkout(this: This, cx: &mut AsyncWindowContext, dir: PathBuf, number: u32) -> anyhow::Result<()> {
    let n = [Param::Number(f64::from(number))];
    let started = this.update(cx, |workbench, cx| {
        let progress = cx.default_global::<Localization>().0.tx_built(&["Checking out #", "…"], &n);
        workbench.begin_outside_git_operation(&progress, cx)
    })?;
    let Ok(mark) = started else { return Ok(()) };
    save_all(&this, cx).await?;
    // A read of the repository still running would hold the index lock the checkout takes.
    refresh(&this, cx).await?;
    // gh runs git: it holds the window's gate as the window's own Git commands do.
    let command = git_command_starts(&this, cx).await?;
    let work = move |cli: &GhCli| {
        let cancel = CancellationToken::new();
        let output = |line: String| tracing::debug!(line = %redacted_text(&line), "gh pr checkout");
        cli.checkout(&dir, number, &cancel, &output).map_err(|error| error.message)
    };
    let run = this.update(cx, |workbench, cx| gh_call(cx, &workbench.label, "gh_pr_checkout", work))?;
    let result = run.await;
    git_command_ended(&this, cx, command)?;
    refresh(&this, cx).await?;
    this.update(cx, |workbench, cx| {
        workbench.end_outside_git_operation(mark);
        let i18n = &mut cx.default_global::<Localization>().0;
        let request = match result {
            Ok(()) => {
                let title = i18n.tx_built(&["Checked out pull request #", ""], &n);
                github_notice(NotificationLevel::Info, title, "github-checkout")
            }
            Err(error) => {
                let title = i18n.tx_built(&["Couldn't check out #", ""], &n);
                let mut request = github_notice(NotificationLevel::Error, title, "github-op");
                request.body = Some(redacted_text(&error));
                request
            }
        };
        workbench.notify(request, cx);
        workbench.github_vcs_changed(cx);
    })
}

// -------------------------------------------------------------------------------------------------
// Actions

/// Registers the actions of the Pull Requests window that run here (`src/features/github/index.ts`,
/// their metadata from the manifest).
pub(super) fn register_actions(registry: &mut ActionRegistry) {
    registry.handle("github.pullRequests", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.show_pull_requests(window, cx))
    });
    registry.handle("github.openInBrowser", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.open_github_in_browser(window, cx))
    });
    registry.handle("github.checkoutPullRequest", |call, window, cx| {
        let number = number_arg(call.arg);
        on_workbench(window, cx, move |workbench, window, cx| {
            workbench.show_pull_requests(window, cx);
            match number.or_else(|| workbench.context_pull_request(cx)) {
                Some(number) => workbench.checkout_pull_request(number, window, cx),
                None => tracing::info!("github: Checkout Pull Request… without a pull request needs a number prompt"),
            }
        })
    });
    registry.handle("github.pullRequest.checkoutBranch", |call, window, cx| {
        let number = number_arg(call.arg);
        on_workbench(window, cx, move |workbench, window, cx| {
            if let Some(number) = number.or_else(|| workbench.context_pull_request(cx)) {
                workbench.checkout_pull_request(number, window, cx);
            }
        })
    });
    registry.handle("github.refresh", |_, window, cx| {
        on_workbench(window, cx, |workbench, _, cx| workbench.refresh_pull_request_list(cx))
    });
    registry.handle("github.changeRepository", |_, window, cx| {
        on_workbench(window, cx, |workbench, window, cx| workbench.change_github_repository(window, cx))
    });
    registry.handle("github.pullRequest.show", |call, window, cx| {
        let number = number_arg(call.arg);
        on_workbench(window, cx, move |workbench, window, cx| {
            if let Some(number) = number.or_else(|| workbench.context_pull_request(cx)) {
                workbench.open_pull_request(number, true, window, cx);
            }
        })
    });
    registry.handle("github.pullRequest.refresh", |call, window, cx| {
        let number = number_arg(call.arg);
        on_workbench(window, cx, move |workbench, _, cx| {
            if let Some(number) = number.or_else(|| workbench.context_pull_request(cx)) {
                workbench.load_pull_request(number, cx);
            }
        })
    });
    registry.handle("github.pullRequest.openInBrowser", |call, window, cx| {
        let number = number_arg(call.arg);
        on_workbench(window, cx, move |workbench, _, cx| {
            if let Some(number) = number.or_else(|| workbench.context_pull_request(cx)) {
                workbench.with_pull_request_url(number, cx, |url, cx| cx.open_url(&url));
            }
        })
    });
    registry.handle("github.pullRequest.copyUrl", |call, window, cx| {
        let number = number_arg(call.arg);
        on_workbench(window, cx, move |workbench, _, cx| {
            if let Some(number) = number.or_else(|| workbench.context_pull_request(cx)) {
                workbench.with_pull_request_url(number, cx, |url, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url));
                });
            }
        })
    });
}

impl Workbench {
    /// The Pull Requests window, once it was shown.
    pub fn pull_requests_window(&self) -> Option<&Entity<PullRequestsWindow>> {
        self.github.as_ref().map(|slot| &slot.window)
    }

    /// The details panel of pull request `number`'s tab, while it is open.
    pub fn pull_request_details(&self, number: u32) -> Option<&Entity<DetailsPanel>> {
        self.github.as_ref().and_then(|slot| slot.tabs.get(&number)).map(|tab| &tab.panel)
    }

    /// The connection as the workbench keeps it (the window shows a copy).
    pub fn github_repository(&self) -> Option<&RepositoryState> {
        self.github.as_ref().map(|slot| &slot.repo)
    }

    /// The tool window's panel; the window and its slot are made on first use.
    pub(super) fn mount_pull_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Panel> {
        if self.github.is_none() {
            let root = self.project_root(cx)?.to_string_lossy().into_owned();
            let view = cx.new(|cx| PullRequestsWindow::new(&root, cx));
            // Subscribed before anything reaches the window: its first connect asks for a page at once.
            let events = cx.subscribe_in(&view, window, Self::pull_requests_event);
            self.github = Some(GithubSlot {
                root,
                primary: None,
                repo: RepositoryState::default(),
                data: RepositoryData::default(),
                busy: FlowBusy::default(),
                window: view,
                tabs: HashMap::new(),
                shown: false,
                reload_on_connect: false,
                refreshing: None,
                connecting: None,
                list_fetch: None,
                _events: events,
            });
        }
        let view = self.github.as_ref()?.window.clone();
        Some(Panel::new(view, None))
    }

    /// The project goes: its connection, its tabs and what they load go with it.
    pub(super) fn drop_github(&mut self) {
        self.github = None;
    }

    /// Follows the tool window's visibility: gh is checked when it is first shown, and the list is
    /// refreshed when it is shown again.
    pub(super) fn sync_pull_requests_window(&mut self, cx: &mut Context<Self>) {
        let visible = self.is_tool_window_visible(PULL_REQUESTS_TOOL_WINDOW);
        let Some(slot) = self.github.as_mut().filter(|slot| slot.shown != visible) else { return };
        slot.shown = visible;
        if visible {
            let view = slot.window.clone();
            view.update(cx, |view, cx| view.shown(cx));
        }
    }

    /// Publishes `githubConnected` and `githubPrTab` (the active tab: `list`, `pr` or `create`).
    pub(super) fn publish_github_context(&self, window: &mut Window, cx: &mut App) {
        let (connected, tab) = match &self.github {
            Some(slot) => (slot.repo.connection.is_some(), slot.window.read(cx).tabs().active.context()),
            None => (false, GhTab::List.context()),
        };
        set_context(window, cx, "githubConnected", connected);
        set_context(window, cx, "githubPrTab", tab);
    }

    /// The repository was read again: the open tabs say again whether their branch is checked out.
    pub(super) fn github_vcs_changed(&mut self, cx: &mut Context<Self>) {
        let numbers: Vec<u32> =
            self.github.as_ref().map(|slot| slot.tabs.keys().copied().collect()).unwrap_or_default();
        for number in numbers {
            self.push_pull_request_inputs(number, cx);
        }
    }

    /// View Pull Requests: shows the window (checking gh the first time); a window already shown
    /// refreshes its list.
    pub(super) fn show_pull_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_visible = self.is_tool_window_visible(PULL_REQUESTS_TOOL_WINDOW);
        self.with_layout(window, cx, |layout| layout.show(PULL_REQUESTS_TOOL_WINDOW, true));
        if was_visible && let Some(view) = self.pull_requests_window().cloned() {
            view.update(cx, |view, cx| view.shown(cx));
        }
    }

    /// Open on GitHub: the page of the pull request the window points at, else of the repository.
    fn open_github_in_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_pull_requests(window, cx);
        if let Some(number) = self.context_pull_request(cx) {
            self.with_pull_request_url(number, cx, |url, cx| cx.open_url(&url));
            return;
        }
        let connection = self.github.as_ref().and_then(|slot| slot.repo.connection.as_ref());
        if let Some(url) = connection.map(|connection| connection.repository.url.clone()).filter(|url| !url.is_empty())
        {
            cx.open_url(&url);
        }
    }

    /// The pull request the actions apply to: the active `#N` tab's, else the list's selection.
    fn context_pull_request(&self, cx: &App) -> Option<u32> {
        self.pull_requests_window()?.read(cx).context_pull_request()
    }

    /// Opens (or selects) pull request `number`'s tab, showing the window.
    fn open_pull_request(&mut self, number: u32, timeline: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.show_pull_requests(window, cx);
        if let Some(view) = self.pull_requests_window().cloned() {
            view.update(cx, |view, cx| view.open_pull_request(number, timeline, cx));
        }
    }

    /// The web page of pull request `number` as gh gave it already: in its details or its list row.
    fn known_pull_request_url(&self, number: u32, cx: &App) -> Option<String> {
        let slot = self.github.as_ref()?;
        let loaded = slot.tabs.get(&number).and_then(|tab| tab.details.as_ref()).map(|pr| pr.short.url.clone());
        let listed = || {
            let list = slot.window.read(cx).list().read(cx);
            list.list().items.iter().find(|pr| pr.number == number).map(|pr| pr.url.clone())
        };
        loaded.filter(|url| !url.is_empty()).or_else(|| listed().filter(|url| !url.is_empty()))
    }

    /// Runs `then` with the web page of pull request `number`: the one gh gave already, else the one
    /// in the details gh gives for it once they arrive. The page a repository's name would build can
    /// be wrong (a renamed repository), so none is built; when gh fails, nothing runs.
    fn with_pull_request_url(
        &self,
        number: u32,
        cx: &mut Context<Self>,
        then: impl FnOnce(String, &mut App) + 'static,
    ) {
        if let Some(url) = self.known_pull_request_url(number, cx) {
            then(url, cx);
            return;
        }
        let Some(slot) = self.github.as_ref() else { return };
        let Some(connection) = slot.repo.connection.as_ref() else { return };
        let repo = connection.coordinates.clone();
        let dir = gh_dir(slot.repo.connected_root(), slot);
        let details = gh_call(cx, &self.label, "gh_api", move |cli| find_pull_request(cli, &dir, &repo, number));
        cx.spawn(async move |_, cx| match details.await {
            Ok(pr) if !pr.short.url.is_empty() => {
                cx.update(|cx| then(pr.short.url, cx));
            }
            Ok(_) => tracing::warn!(number, "gh gave no page for the pull request"),
            Err(error) => tracing::warn!(number, "the pull request's page did not load: {}", redacted_text(&error)),
        })
        .detach();
    }

    /// The window shows the connection as it is now.
    fn push_github_repository(&mut self, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_ref() else { return };
        let (view, repo) = (slot.window.clone(), slot.repo.clone());
        view.update(cx, |view, cx| view.set_repository(repo, cx));
        cx.notify();
    }

    /// Carries out what a disconnect asks: the saved choice and the cached lists go.
    fn github_disconnected(&mut self, disconnected: Disconnected, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        slot.reload_on_connect = false;
        if disconnected.clear_selection {
            cx.default_global::<UiStorage>().remove(&selection_key(&slot.root));
        }
        if disconnected.reset_data {
            slot.data.reset();
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Connecting

    /// Checks gh and the project's GitHub remotes again, then connects when the choice is clear.
    fn refresh_github(&mut self, cx: &mut Context<Self>) {
        let label = self.label.clone();
        let Some(slot) = self.github.as_mut() else { return };
        let ticket = slot.repo.begin_refresh();
        let listing =
            cx.try_global::<Backend>().map(|backend| backend.with_host(move |host| list_remotes(host, label)));
        slot.refreshing = Some(cx.spawn(async move |this, cx| {
            let listed = match listing {
                None => Err("The backend is not running".to_owned()),
                Some(listing) => match listing.await {
                    Ok(Ok(listed)) => Ok(listed),
                    Ok(Err(error)) => Err(error.message),
                    Err(error) => Err(error.to_string()),
                },
            };
            // The window is gone: nothing is shown any more.
            let _ = this.update(cx, |workbench, cx| workbench.github_remotes_listed(ticket, listed, cx));
        }));
        self.push_github_repository(cx);
    }

    /// Refresh Pull Requests List: gh and the connection are checked again, then the list loads again.
    fn refresh_pull_request_list(&mut self, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        slot.reload_on_connect = true;
        self.refresh_github(cx);
    }

    fn github_remotes_listed(
        &mut self,
        ticket: RefreshTicket,
        listed: Result<(Vec<RootRemotes>, Option<String>), String>,
        cx: &mut Context<Self>,
    ) {
        let (lists, primary) = listed.unwrap_or_else(|error| {
            // gh is still asked: it says what it finds in the project's folder.
            tracing::warn!("the project's remotes could not be listed: {}", redacted_text(&error));
            (Vec::new(), None)
        });
        let Some(slot) = self.github.as_mut() else { return };
        let saved = cx.try_global::<UiStorage>().and_then(|storage| storage.get(&selection_key(&slot.root)));
        let saved = saved.map(str::to_owned);
        let Some(request) = slot.repo.remotes_listed(&ticket, lists, saved.as_deref()) else { return };
        slot.primary = primary;
        let dir = gh_dir(request.probe.as_deref(), slot);
        let call = gh_call(cx, &self.label, "gh_availability", move |cli| Ok(cli.availability(&dir)));
        slot.refreshing = Some(cx.spawn(async move |this, cx| {
            let answer = call.await;
            let _ = this.update(cx, |workbench, cx| workbench.github_availability_answered(request, answer, cx));
        }));
    }

    fn github_availability_answered(
        &mut self,
        request: AvailabilityRequest,
        answer: Result<GhAvailability, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.github.as_mut() else { return };
        match slot.repo.availability_answered(request, answer) {
            RefreshStep::Connect(connect) => {
                self.push_github_repository(cx);
                self.connect_github(connect, cx);
            }
            RefreshStep::NotConnected { disconnected } => {
                self.github_disconnected(disconnected, cx);
                self.push_github_repository(cx);
            }
            RefreshStep::Stale => {}
        }
    }

    /// Finds the repository and the signed-in user for `request`.
    fn connect_github(&mut self, request: ConnectRequest, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        let dir = gh_dir(request.root.as_deref(), slot);
        let repo = request.coordinates.clone();
        let call = gh_call(cx, &self.label, "gh_api", move |cli| find_repository_and_user(cli, &dir, &repo));
        slot.connecting = Some(cx.spawn(async move |this, cx| {
            let answer = call.await;
            let _ = this.update(cx, |workbench, cx| workbench.github_connected(request, answer, cx));
        }));
        self.push_github_repository(cx);
    }

    fn github_connected(
        &mut self,
        request: ConnectRequest,
        answer: Result<(Option<GhRepository>, GhUser), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.github.as_mut() else { return };
        match slot.repo.connected(request, answer) {
            ConnectOutcome::Connected { save_selection, reset_data } => {
                if let Some(name) = save_selection {
                    cx.default_global::<UiStorage>().set(&selection_key(&slot.root), name);
                }
                if reset_data {
                    slot.data.reset();
                }
                let reload = std::mem::take(&mut slot.reload_on_connect);
                self.push_github_repository(cx);
                if reload {
                    self.reload_pull_request_list(cx);
                }
            }
            ConnectOutcome::Failed => {
                slot.reload_on_connect = false;
                self.push_github_repository(cx);
            }
            ConnectOutcome::Stale => {}
        }
    }

    /// Change Repository or Account…: disconnects and forgets the choice, or (not connected) shows
    /// the window and checks again.
    fn change_github_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else {
            self.show_pull_requests(window, cx);
            return;
        };
        match change_repository(&slot.repo) {
            ChangeRepository::Disconnect => {
                let disconnected = slot.repo.disconnect(true);
                self.github_disconnected(disconnected, cx);
                self.push_github_repository(cx);
            }
            ChangeRepository::ShowToolWindow => {
                self.show_pull_requests(window, cx);
                self.refresh_github(cx);
            }
        }
    }

    // ---------------------------------------------------------------------------------------------
    // The window's events

    fn pull_requests_event(
        &mut self,
        _: &Entity<PullRequestsWindow>,
        event: &PullRequestsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            PullRequestsEvent::Refresh => self.refresh_github(cx),
            PullRequestsEvent::Connect(coordinates) => {
                let Some(slot) = self.github.as_mut() else { return };
                let request = slot.repo.connect(&coordinates, None, true);
                self.connect_github(request, cx);
            }
            PullRequestsEvent::OpenUrl(url) => cx.open_url(&url),
            // Run gh auth login: a Terminal tab runs it, where gh asks the user what it needs.
            PullRequestsEvent::RunAction { id: TERMINAL_RUN_COMMAND, arg } => run_in_terminal(arg, window, cx),
            PullRequestsEvent::RunAction { id, arg } => {
                if cx.try_global::<ActionRegistry>().is_some_and(|registry| registry.contains(id)) {
                    run_action_later(id.to_owned(), arg, window, cx);
                } else {
                    tracing::info!("{id} is not available in this window");
                }
            }
            PullRequestsEvent::PullRequestOpened { number, timeline } => {
                self.open_pull_request_tab(number, window, cx);
                if timeline {
                    tracing::info!(number, "github: the timeline editor of PullRequestOpened is not built");
                }
            }
            PullRequestsEvent::CreateOpened => {
                tracing::info!("github: CreateOpened needs the New Pull Request view, which is not built");
            }
            PullRequestsEvent::TabClosed(GhTab::PullRequest(number)) => {
                if let Some(slot) = self.github.as_mut() {
                    slot.tabs.remove(&number);
                }
            }
            PullRequestsEvent::TabClosed(_) => {}
            // `githubPrTab` is published with the window's other context.
            PullRequestsEvent::ActiveTabChanged(_) => cx.notify(),
            PullRequestsEvent::ShowMenu { position, items } => {
                let Some(view) = self.pull_requests_window().map(Entity::downgrade) else { return };
                let run = move |_: &mut Self, command, _: &mut Window, cx: &mut Context<Self>| {
                    // The window is gone with its project.
                    let _ = view.update(cx, |view, cx| view.run_command(command, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
            PullRequestsEvent::List(event) => self.pull_request_list_event(event, window, cx),
        }
    }

    fn pull_request_list_event(&mut self, event: ListPanelEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            ListPanelEvent::Fetch(request) => self.fetch_pull_requests(request, cx),
            ListPanelEvent::LoadChooser(chip) => self.load_chooser(chip, cx),
            ListPanelEvent::ShowMenu { position, items } => {
                let Some(list) = self.pull_requests_window().map(|view| view.read(cx).list().downgrade()) else {
                    return;
                };
                let run = move |_: &mut Self, command, _: &mut Window, cx: &mut Context<Self>| {
                    let _ = list.update(cx, |list, cx| list.run_command(command, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
            // The window stores the searches and follows the selection and Open itself.
            ListPanelEvent::SaveLastSearch(_)
            | ListPanelEvent::SaveHistory(_)
            | ListPanelEvent::Selected(_)
            | ListPanelEvent::Open(_) => {}
        }
    }

    // ---------------------------------------------------------------------------------------------
    // The list

    /// Sends a page request of the list; the answer goes to the list with its ticket.
    fn fetch_pull_requests(&mut self, request: ListRequest, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        let dir = gh_dir(slot.repo.connected_root(), slot);
        let ListRequest { ticket, request } = request;
        let list = slot.window.read(cx).list().clone();
        let call = gh_call(cx, &self.label, "gh_api", move |cli| gh_api(cli, &dir, &request));
        slot.list_fetch = Some(cx.spawn(async move |this, cx| {
            let answer = call.await;
            let _ = this.update(cx, |_, cx| list.update(cx, |list, cx| list.search_answered(ticket, answer, cx)));
        }));
    }

    /// The current search again from the first page.
    fn reload_pull_request_list(&mut self, cx: &mut Context<Self>) {
        let Some(list) = self.pull_requests_window().map(|view| view.read(cx).list().clone()) else { return };
        list.update(cx, |list, cx| list.reload(cx));
    }

    /// Loads the items of the chooser of `chip` (once per repository), page by page.
    fn load_chooser(&mut self, chip: Chip, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        let Some(permission) = slot.repo.connection.as_ref().map(|c| c.repository.viewer_permission) else { return };
        let Some(source) = ChooserSource::of(chip, permission) else { return };
        if let LoadStep::Fetch(ticket) = source.load(&mut slot.data) {
            self.fetch_chooser_page(chip, source, ticket, cx);
        }
        self.push_chooser(chip, cx);
    }

    fn fetch_chooser_page(
        &mut self,
        chip: Chip,
        source: ChooserSource,
        ticket: PageTicket<u32>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.github.as_ref() else { return };
        let Some(repo) = slot.repo.connection.as_ref().map(|c| c.coordinates.clone()) else { return };
        let dir = gh_dir(slot.repo.connected_root(), slot);
        let request = rest_page_request(&repo, source.list(), ticket.cursor);
        let call = gh_call(cx, &self.label, "gh_api", move |cli| gh_api(cli, &dir, &request));
        cx.spawn(async move |this, cx| {
            let answer = call.await;
            let _ = this.update(cx, |workbench, cx| workbench.chooser_page_loaded(chip, source, ticket, answer, cx));
        })
        .detach();
    }

    fn chooser_page_loaded(
        &mut self,
        chip: Chip,
        source: ChooserSource,
        ticket: PageTicket<u32>,
        answer: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.github.as_mut() else { return };
        match source.page_loaded(&mut slot.data, &ticket, answer) {
            PageOutcome::Fetch(next) => self.fetch_chooser_page(chip, source, next, cx),
            PageOutcome::Stale => return,
            PageOutcome::Done | PageOutcome::Failed(_) => {}
        }
        self.push_chooser(chip, cx);
    }

    /// Gives the chooser of `chip` its items as loaded so far.
    fn push_chooser(&mut self, chip: Chip, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_ref() else { return };
        let Some(permission) = slot.repo.connection.as_ref().map(|c| c.repository.viewer_permission) else { return };
        let (items, loading, error) = match chip {
            Chip::Author => {
                let state = slot.data.author_candidates(permission);
                (user_items(&state.items), state.loading, state.error)
            }
            Chip::Assignee => {
                let state = slot.data.assignees.state();
                (user_items(&state.items), state.loading, state.error.clone())
            }
            Chip::Label => {
                let state = slot.data.labels.state();
                (label_items(&state.items), state.loading, state.error.clone())
            }
            Chip::State | Chip::Review | Chip::Sort => return,
        };
        let list = slot.window.read(cx).list().clone();
        list.update(cx, |list, cx| list.set_chooser_items(chip, items, loading, error, cx));
    }

    // ---------------------------------------------------------------------------------------------
    // A pull request's tab

    /// The `#N` tab opened or was selected: it gets its panel once, and its details load again.
    fn open_pull_request_tab(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        if !slot.tabs.contains_key(&number) {
            let panel = cx.new(|cx| DetailsPanel::new(number, cx));
            let events = cx.subscribe_in(&panel, window, move |workbench, _, event: &DetailsEvent, window, cx| {
                workbench.pull_request_details_event(number, event, window, cx);
            });
            let view = slot.window.clone();
            let shown = panel.clone();
            view.update(cx, |view, cx| view.set_tab_view(GhTab::PullRequest(number), shown.into(), cx));
            slot.tabs.insert(
                number,
                PullRequestTab {
                    panel,
                    details: None,
                    mergeability: None,
                    commits: Vec::new(),
                    files: None,
                    viewed: HashMap::new(),
                    commit: None,
                    generation: 0,
                    loading: None,
                    commit_loading: None,
                    _events: events,
                },
            );
        }
        self.load_pull_request(number, cx);
    }

    /// Loads pull request `number`'s details, mergeability, commits, changes and viewed states; the
    /// requests run at once and each answer shows as it arrives.
    fn load_pull_request(&mut self, number: u32, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        let Some(connection) = slot.repo.connection.as_ref() else { return };
        let repo = connection.coordinates.clone();
        let admin = connection.repository.viewer_permission == Some(Permission::Admin);
        let dir = gh_dir(slot.repo.connected_root(), slot);
        let Some(tab) = slot.tabs.get_mut(&number) else { return };
        tab.generation += 1;
        let generation = tab.generation;
        let details = {
            let (dir, repo) = (dir.clone(), repo.clone());
            gh_call(cx, &self.label, "gh_api", move |cli| find_pull_request(cli, &dir, &repo, number))
        };
        let mergeability = {
            let (dir, repo) = (dir.clone(), repo.clone());
            gh_call(cx, &self.label, "gh_api", move |cli| {
                find_mergeability(cli, &dir, &repo, number).map(|data| build_mergeability_state(&data, admin))
            })
        };
        let commits = {
            let (dir, repo) = (dir.clone(), repo.clone());
            gh_call(cx, &self.label, "gh_api", move |cli| {
                let nodes: Vec<CommitNode> =
                    pull_request_pages(cli, &dir, (&repo, number), "pullRequestCommits", "commits")?;
                Ok(nodes.into_iter().map(|node| node.commit).collect::<Vec<_>>())
            })
        };
        let files = {
            let (dir, repo) = (dir.clone(), repo.clone());
            gh_call(cx, &self.label, "gh_api", move |cli| pull_request_files(cli, &dir, &repo, number))
        };
        let viewed = gh_call(cx, &self.label, "gh_api", move |cli| viewed_states(cli, &dir, &repo, number));
        tab.loading = Some(cx.spawn(async move |this, cx| {
            let details = details.await;
            let shown = this.update(cx, |workbench, cx| {
                workbench.pull_request_details_loaded(number, generation, details, cx);
            });
            // The window is gone: nothing more is shown.
            if shown.is_err() {
                return;
            }
            let mergeability = mergeability.await;
            let apply = move |tab: &mut PullRequestTab| match mergeability {
                Ok(state) => tab.mergeability = Some(state),
                // The status section keeps saying it loads; the reason is in the log.
                Err(error) => tracing::warn!(number, "the mergeability did not load: {}", redacted_text(&error)),
            };
            if this.update(cx, |workbench, cx| workbench.pull_request_loaded(number, generation, apply, cx)).is_err() {
                return;
            }
            let commits = commits.await;
            let apply = move |tab: &mut PullRequestTab| match commits {
                Ok(commits) => tab.commits = commits,
                Err(error) => tracing::warn!(number, "the commits did not load: {}", redacted_text(&error)),
            };
            if this.update(cx, |workbench, cx| workbench.pull_request_loaded(number, generation, apply, cx)).is_err() {
                return;
            }
            let files = files.await;
            let apply = move |tab: &mut PullRequestTab| tab.files = Some(files);
            if this.update(cx, |workbench, cx| workbench.pull_request_loaded(number, generation, apply, cx)).is_err() {
                return;
            }
            let viewed = viewed.await;
            let apply = move |tab: &mut PullRequestTab| match viewed {
                Ok(viewed) => tab.viewed = viewed,
                Err(error) => tracing::warn!(number, "the viewed states did not load: {}", redacted_text(&error)),
            };
            let _ = this.update(cx, |workbench, cx| workbench.pull_request_loaded(number, generation, apply, cx));
        }));
    }

    fn pull_request_details_loaded(
        &mut self,
        number: u32,
        generation: u64,
        details: Result<GhPullRequest, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.github.as_mut() else { return };
        let Some(tab) = slot.tabs.get_mut(&number).filter(|tab| tab.generation == generation) else { return };
        tab.details = details.as_ref().ok().cloned();
        let (panel, view) = (tab.panel.clone(), slot.window.clone());
        if let Ok(pr) = &details {
            let (title, short) = (pr.short.title.clone(), pr.short.clone());
            view.update(cx, |view, cx| {
                view.set_title(number, &title, cx);
                view.list().clone().update(cx, |list, cx| list.update_item(&short, cx));
            });
        }
        panel.update(cx, |panel, cx| panel.set_details(details, cx));
        self.push_pull_request_inputs(number, cx);
    }

    /// Applies an answer of the current load of tab `number` and shows it.
    fn pull_request_loaded(
        &mut self,
        number: u32,
        generation: u64,
        apply: impl FnOnce(&mut PullRequestTab),
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.github.as_mut() else { return };
        let Some(tab) = slot.tabs.get_mut(&number).filter(|tab| tab.generation == generation) else { return };
        apply(tab);
        self.push_pull_request_inputs(number, cx);
    }

    /// Gives tab `number`'s panel what it shows besides the details.
    fn push_pull_request_inputs(&mut self, number: u32, cx: &mut Context<Self>) {
        let branch = self.vcs_state().and_then(|state| state.branch.clone());
        let merge_in_progress = branch.as_ref().is_some_and(|branch| branch.operation.is_some());
        let Some(slot) = self.github.as_ref() else { return };
        let Some(connection) = slot.repo.connection.as_ref() else { return };
        let Some(tab) = slot.tabs.get(&number) else { return };
        let i18n = &mut cx.default_global::<Localization>().0;
        let resolve_error =
            resolve_conflicts(tab.details.as_ref(), &connection.remotes, merge_in_progress, false, i18n).err();
        let checked_out =
            tab.details.as_ref().is_some_and(|pr| is_head_checked_out(branch.as_ref(), &connection.remotes, pr));
        let files = match &tab.commit {
            Some((_, files)) => files.clone(),
            None => tab.files.clone(),
        };
        let inputs = DetailsInputs {
            me: connection.current_user.clone(),
            repository: connection.repository.clone(),
            mergeability: tab.mergeability.clone(),
            commits: tab.commits.clone(),
            files,
            viewed: tab.viewed.clone(),
            threads: Vec::new(),
            checked_out,
            resolve_error,
            pending_comments: 0,
            busy: slot.busy.is_busy(number),
        };
        let panel = tab.panel.clone();
        panel.update(cx, |panel, cx| panel.set_inputs(inputs, cx));
    }

    fn pull_request_details_event(
        &mut self,
        number: u32,
        event: &DetailsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            DetailsEvent::Reload => self.load_pull_request(number, cx),
            DetailsEvent::OpenUrl(url) => cx.open_url(&url),
            DetailsEvent::CommitSelected(oid) => self.select_pull_request_commit(number, oid, cx),
            DetailsEvent::OpenDiff { path, commit } => self.open_pull_request_diff(number, path, commit, window, cx),
            DetailsEvent::Reopen => self.set_pull_request_state(number, "open", cx),
            DetailsEvent::Close => self.set_pull_request_state(number, "closed", cx),
            DetailsEvent::Post => self.mark_ready_for_review(number, cx),
            DetailsEvent::Flow(FlowButton::SetMyselfAsReviewer) => self.set_myself_as_reviewer(number, cx),
            DetailsEvent::AdjustReviewers(chosen) => self.adjust_reviewers(number, chosen, cx),
            DetailsEvent::ShowMenu { position, items } => {
                let Some(panel) = self.pull_request_details(number).map(Entity::downgrade) else { return };
                let run = move |_: &mut Self, command, _: &mut Window, cx: &mut Context<Self>| {
                    let _ = panel.update(cx, |panel, cx| panel.run_command(command, cx));
                };
                self.show_menu(&items, position, run, window, cx);
            }
            DetailsEvent::Flow(button) => {
                tracing::info!(number, ?button, "github: DetailsEvent::Flow needs a view that is not built")
            }
            DetailsEvent::Merge(method) => {
                tracing::info!(number, ?method, "github: DetailsEvent::Merge needs the merge dialog, not built")
            }
            DetailsEvent::RequestReview => {
                tracing::info!(number, "github: DetailsEvent::RequestReview needs the reviewer picker, not built")
            }
            DetailsEvent::ResolveConflicts => {
                tracing::info!(number, "github: DetailsEvent::ResolveConflicts is not built")
            }
            DetailsEvent::ShowTimeline(n) => {
                tracing::info!(number = n, "github: DetailsEvent::ShowTimeline needs the timeline editor, not built")
            }
        }
    }

    /// The commit picker chose `oid` (`None`: every commit): the tree shows that commit's changes.
    fn select_pull_request_commit(&mut self, number: u32, oid: Option<String>, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        let Some(repo) = slot.repo.connection.as_ref().map(|c| c.coordinates.clone()) else { return };
        let dir = gh_dir(slot.repo.connected_root(), slot);
        let Some(tab) = slot.tabs.get_mut(&number) else { return };
        tab.commit = oid.clone().map(|oid| (oid, None));
        tab.commit_loading = oid.map(|oid| {
            let shown = oid.clone();
            let call = gh_call(cx, &self.label, "gh_api", move |cli| commit_files(cli, &dir, &repo, &oid));
            cx.spawn(async move |this, cx| {
                let files = call.await;
                let _ = this.update(cx, |workbench, cx| {
                    let Some(tab) = workbench.github.as_mut().and_then(|slot| slot.tabs.get_mut(&number)) else {
                        return;
                    };
                    // Another commit was chosen meanwhile.
                    if tab.commit.as_ref().is_some_and(|(chosen, _)| *chosen == shown) {
                        tab.commit = Some((shown, Some(files)));
                        workbench.push_pull_request_inputs(number, cx);
                    }
                });
            })
        });
        self.push_pull_request_inputs(number, cx);
    }

    /// Fetches what the diff needs, then shows `path`'s diff in the pull request (or `commit`).
    fn open_pull_request_diff(
        &mut self,
        number: u32,
        path: String,
        commit: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.github.as_ref() else { return };
        let Some(name_with_owner) = slot.repo.connection.as_ref().map(|c| c.coordinates.name_with_owner()) else {
            return;
        };
        let Some(pr) = slot.tabs.get(&number).and_then(|tab| tab.details.clone()) else {
            tracing::info!(number, "github: a diff is asked for before the details loaded");
            return;
        };
        let dir = gh_dir(slot.repo.connected_root(), slot);
        // Only git runs here (it fetches what is missing from the connected remote).
        let call = gh_call(cx, &self.label, "gh_pr_prepare_diff", move |_| {
            let cancel = CancellationToken::new();
            let output = |line: String| tracing::debug!(line = %redacted_text(&line), "preparing a pull request diff");
            ide_github::prepare_diff(
                &dir,
                number,
                &pr.head_ref_oid,
                &pr.base_ref_oid,
                &pr.base_ref_name,
                &name_with_owner,
                &cancel,
                &output,
            )
            .map_err(|error| error.message)
        });
        cx.spawn_in(window, async move |this, cx| {
            let prepared = call.await;
            let _ = this.update_in(cx, |workbench, window, cx| match prepared {
                Ok(revisions) => {
                    workbench.open_diff(pull_request_diff(number, &path, commit.as_deref(), &revisions), window, cx);
                }
                Err(error) => {
                    let request = failure("Cannot show the pull request diff", &error, cx);
                    workbench.notify(request, cx);
                }
            });
        })
        .detach();
    }

    /// Checkout: `gh pr checkout` in the connected repository, then the repository is read again.
    fn checkout_pull_request(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_ref().filter(|slot| slot.repo.connection.is_some()) else { return };
        let dir = gh_dir(slot.repo.connected_root(), slot);
        cx.spawn_in(window, async move |this, cx| finished(run_checkout(this, cx, dir, number).await)).detach();
    }

    // ---------------------------------------------------------------------------------------------
    // The review flow's mutations

    fn set_pull_request_state(&mut self, number: u32, state: &str, cx: &mut Context<Self>) {
        let Some(repo) = self.github.as_ref().and_then(|s| s.repo.connection.as_ref()).map(|c| c.coordinates.clone())
        else {
            return;
        };
        let endpoint = format!("{}/pulls/{number}", repo_path(&repo));
        let request = rest_request("PATCH", &endpoint, Some(&json!({ "state": state })), false);
        let progress = if state == "open" { "Reopening pull request…" } else { "Closing pull request…" };
        self.mutate_pull_request(number, progress, vec![request], cx);
    }

    /// Post Pull Request: the draft becomes ready for review.
    fn mark_ready_for_review(&mut self, number: u32, cx: &mut Context<Self>) {
        let id = self.github.as_ref().and_then(|s| s.tabs.get(&number)).and_then(|t| t.details.as_ref());
        let Some(id) = id.map(|pr| pr.short.id.clone()) else { return };
        match document_request("markPullRequestReadyForReview", &[("pullRequestId", Some(Value::from(id)))]) {
            Ok(request) => {
                self.mutate_pull_request(number, "Marking pull request ready for review…", vec![request], cx);
            }
            Err(error) => {
                let request = failure("Pull request action failed", &error, cx);
                self.notify(request, cx);
            }
        }
    }

    fn set_myself_as_reviewer(&mut self, number: u32, cx: &mut Context<Self>) {
        let Some(connection) = self.github.as_ref().and_then(|s| s.repo.connection.as_ref()) else { return };
        let endpoint = format!("{}/pulls/{number}/requested_reviewers", repo_path(&connection.coordinates));
        let me = GhReviewer::Actor(connection.current_user.clone());
        let request = rest_request("POST", &endpoint, Some(&reviewers_body(&[me])), false);
        self.mutate_pull_request(number, "Requesting review…", vec![request], cx);
    }

    /// Requests exactly `chosen`: the reviewers added are requested, the ones dropped removed.
    fn adjust_reviewers(&mut self, number: u32, chosen: Vec<GhReviewer>, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        let Some(repo) = slot.repo.connection.as_ref().map(|c| c.coordinates.clone()) else { return };
        let Some(pr) = slot.tabs.get(&number).and_then(|tab| tab.details.as_ref()) else { return };
        let current: Vec<GhReviewer> =
            pr.short.review_requests.nodes.iter().filter_map(|r| r.requested_reviewer.clone()).collect();
        let i18n = &cx.default_global::<Localization>().0;
        let Some(delta) = slot.busy.adjust_reviewers(number, &current, &chosen, i18n) else { return };
        let endpoint = format!("{}/pulls/{number}/requested_reviewers", repo_path(&repo));
        let mut requests = Vec::new();
        if !delta.added.is_empty() {
            requests.push(rest_request("POST", &endpoint, Some(&reviewers_body(&delta.added)), false));
        }
        if !delta.removed.is_empty() {
            requests.push(rest_request("DELETE", &endpoint, Some(&reviewers_body(&delta.removed)), false));
        }
        self.run_pull_request_mutation(number, requests, cx);
    }

    /// Starts a review flow action of pull request `number` unless another one of it runs.
    fn mutate_pull_request(
        &mut self,
        number: u32,
        progress: &str,
        requests: Vec<GhApiRequest>,
        cx: &mut Context<Self>,
    ) {
        let progress = cx.default_global::<Localization>().0.tx(progress);
        let Some(slot) = self.github.as_mut() else { return };
        if !slot.busy.start(number, &progress) {
            return;
        }
        self.run_pull_request_mutation(number, requests, cx);
    }

    /// Sends `requests` in turn (the action is already marked as running); once they end, the pull
    /// request and the list load again, and a failure is told.
    fn run_pull_request_mutation(&mut self, number: u32, requests: Vec<GhApiRequest>, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_ref() else { return };
        let dir = gh_dir(slot.repo.connected_root(), slot);
        let call = gh_call(cx, &self.label, "gh_api", move |cli| {
            requests.iter().try_for_each(|request| gh_api(cli, &dir, request).map(drop))
        });
        self.push_pull_request_inputs(number, cx);
        cx.spawn(async move |this, cx| {
            let result = call.await;
            let _ = this.update(cx, |workbench, cx| workbench.pull_request_mutated(number, result, cx));
        })
        .detach();
    }

    fn pull_request_mutated(&mut self, number: u32, result: Result<(), String>, cx: &mut Context<Self>) {
        let Some(slot) = self.github.as_mut() else { return };
        let result = result.map_err(|error| redacted_text(&error));
        let failed = slot.busy.finish(number, result, &cx.default_global::<Localization>().0);
        if let Some(failed) = failed {
            let mut request = NotificationRequest::new(NotificationLevel::Error, failed.title);
            request.body = Some(failed.body);
            request.key = Some(failed.key.to_owned());
            self.notify(request, cx);
        }
        self.load_pull_request(number, cx);
        self.reload_pull_request_list(cx);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::TERMINAL_TOOL_WINDOW;
    use super::super::tests::{
        branch_label, click_on, git_head, git_in, git_project, init_repo, notification_titles, run_action,
        wait_for_notice, wait_long,
    };
    use super::*;
    use crate::github::details::head_branch_label;
    use crate::github::toolwindow::ToolWindowContent;
    use crate::github::window_view::GH_INSTALL_URL;
    use crate::platform::context::{ContextValue, window_context};
    use gpui_kit::{Focusable, TestAppContext, VisualTestContext};
    use std::cell::{Cell, RefCell};
    mod proofs7_github_urls;
    mod proofs9_github;

    const GITHUB_REMOTE: &str = "https://github.com/org/app.git";

    /// A token that must never show up anywhere the window keeps, shows or logs.
    const SENTINEL: &str = "ghp_SENTINEL0123456789abcdefSENTINEL";

    /// A stand-in for the GitHub CLI, run as `sh <this file> <gh's arguments>`: it answers from the
    /// files beside it, logs each call (its arguments, a tab and the request body) to `calls.log`
    /// there, and fails loudly on anything else. It drops the credential variables gh would read and
    /// runs git without the machine's configuration; it reaches no network.
    const FAKE_GH: &str = r##"unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN
D="$(cd "$(dirname "$0")" && pwd -P)"
body=""
case " $* " in *" --input - "*) body="$(cat)" ;; esac
printf '%s\t%s\n' "$*" "$body" >> "$D/calls.log"
answer() {
  if [ -f "$D/$1" ]; then cat "$D/$1"; exit 0; fi
  echo "fake gh: no answer $1" >&2
  exit 1
}
case "$1 $2" in
  "repo view")
    if [ -f "$D/repo-view.err" ]; then cat "$D/repo-view.err" >&2; exit 1; fi
    answer repo-view.json ;;
  "api user")
    if [ -f "$D/repo-view.err" ]; then cat "$D/repo-view.err" >&2; exit 1; fi
    echo me
    exit 0 ;;
  "pr checkout")
    branch="$(cat "$D/checkout-$3")" || exit 1
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null git checkout -q -b "$branch" || exit 1
    exit 0 ;;
  "api --method") ;;
  *) echo "fake gh: unexpected call $*" >&2; exit 1 ;;
esac
case "$4" in
  graphql)
    case "$body" in
      *"query issueSearch"*)
        for f in "$D"/search-*.json; do
          [ -f "$f" ] || continue
          term="${f##*/search-}"
          term="${term%.json}"
          case "$body" in *"($term)"*) cat "$f"; exit 0 ;; esac
        done
        answer search.json ;;
      *"...pullRequestMergeability"*) answer mergeability.json ;;
      *'commits(first: '*) answer commits.json ;;
      *'files(first: '*) answer viewed.json ;;
      *"pullRequest(number:"*) answer pull-request.json ;;
      *"...repository"*) answer repository.json ;;
    esac ;;
  user) answer user.json ;;
  repos/*/pulls/*/files*) answer files.json ;;
  repos/*/labels*|repos/*/assignees*|repos/*/collaborators*|repos/*/contributors*) echo '[]'; exit 0 ;;
esac
echo "fake gh: unexpected call $*" >&2
exit 1
"##;

    /// The stand-in for gh of this test's window: `cli`, nothing started or recorded yet.
    fn stand_in(cli: GhCli, cx: &mut VisualTestContext) {
        let stand_in = GhStandIn { cli, started: Cell::new(0), terminal: RefCell::new(Vec::new()) };
        cx.update(|_, cx| cx.set_global(stand_in));
    }

    /// A trusted project on a repository whose `origin` is a local bare repository, its URL rewritten
    /// to look like `remote` in the repository's own configuration (`url.<remote>.insteadOf`), which
    /// also lets git reach local files only. The fake gh beside the answers `answers` gives (given
    /// the HEAD commit) stands in for gh.
    fn github_project<'a>(
        cx: &'a mut TestAppContext,
        remote: &str,
        answers: impl FnOnce(&str) -> Vec<(&'static str, String)>,
    ) -> (Entity<Workbench>, &'a mut VisualTestContext, [tempfile::TempDir; 2], PathBuf) {
        let remote = remote.to_owned();
        let (workbench, cx, folders, root) = git_project(cx, move |root| {
            init_repo(root);
            let bare = root.join(".git").join("fake-github").join("org").join("app.git");
            std::fs::create_dir_all(&bare).expect("the remote's folder is made");
            git_in(&bare, &["init", "-q", "--bare"]);
            let bare = bare.to_string_lossy().into_owned();
            git_in(root, &["push", "-q", &bare, "main"]);
            git_in(root, &["config", "protocol.allow", "never"]);
            git_in(root, &["config", "protocol.file.allow", "always"]);
            git_in(root, &["config", &format!("url.{remote}.insteadOf"), &bare]);
            git_in(root, &["remote", "add", "origin", &bare]);
            let head = git_in(root, &["rev-parse", "HEAD"]);
            let dir = root.join(".git").join("fake-gh");
            std::fs::create_dir_all(&dir).expect("the answers' folder is made");
            std::fs::write(dir.join("gh"), FAKE_GH).expect("the fake gh is written");
            for (name, text) in answers(&head) {
                std::fs::write(dir.join(name), text).expect("an answer is written");
            }
        });
        let script = root.join(".git").join("fake-gh").join("gh");
        stand_in(GhCli::at("/bin/sh").arg(script.to_string_lossy()), cx);
        (workbench, cx, folders, root)
    }

    fn actor(login: &str) -> Value {
        json!({
            "__typename": "User",
            "id": format!("U_{login}"),
            "login": login,
            "url": format!("https://github.com/{login}"),
            "avatarUrl": "",
            "name": null,
        })
    }

    fn repository() -> Value {
        json!({
            "id": "R_app",
            "owner": { "__typename": "Organization", "id": "O_org", "login": "org" },
            "nameWithOwner": "org/app",
            "viewerPermission": "WRITE",
            "mergeCommitAllowed": true,
            "squashMergeAllowed": true,
            "rebaseMergeAllowed": true,
            "defaultBranchRef": { "name": "main" },
            "isFork": false,
            "url": "https://github.com/org/app",
            "sshUrl": "git@github.com:org/app.git",
        })
    }

    /// An open pull request as the list's `pullRequestInfoShort` reads it.
    fn pull_request(number: u32, title: &str, author: &str, draft: bool, label: &str) -> Value {
        json!({
            "id": format!("PR_{number}"),
            "url": format!("https://github.com/org/app/pull/{number}"),
            "number": number,
            "title": title,
            "state": "OPEN",
            "isDraft": draft,
            "author": actor(author),
            "createdAt": "2026-11-01T10:00:00Z",
            "updatedAt": "2026-11-02T10:00:00Z",
            "isReadByViewer": true,
            "assignees": { "nodes": [] },
            "labels": { "nodes": [{ "id": format!("L_{label}"), "url": "", "name": label, "color": "d73a4a" }] },
            "reviewRequests": { "nodes": [] },
            "reviewThreads": { "nodes": [] },
            "reviews": { "nodes": [] },
            "reactions": { "nodes": [] },
            "mergeable": "MERGEABLE",
            "viewerCanUpdate": true,
            "viewerCanReact": true,
            "viewerDidAuthor": false,
        })
    }

    fn search(nodes: Vec<Value>) -> String {
        let page = json!({ "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes });
        json!({ "data": { "search": page } }).to_string()
    }

    fn in_pull_request(fields: Value) -> String {
        json!({ "data": { "repository": { "pullRequest": fields } } }).to_string()
    }

    /// A connected repository org/app with two open pull requests: #1 by alice (label bug, head
    /// `fix-crash` at `head`, one passing check, one changed file) and the draft #2 by bob (docs).
    fn github_answers(head: &str) -> Vec<(&'static str, String)> {
        let fix = pull_request(1, "Fix the crash on start", "alice", false, "bug");
        let docs = pull_request(2, "Document the setup", "bob", true, "docs");
        let mut details = fix.clone();
        for (key, value) in [
            ("body", json!("It no longer crashes.")),
            ("reviewDecision", Value::Null),
            ("baseRefName", json!("main")),
            ("baseRefOid", json!(head)),
            ("baseRepository", repository()),
            ("headRef", json!({ "id": "REF_1" })),
            ("headRefName", json!("fix-crash")),
            ("headRefOid", json!(head)),
            ("headRepository", repository()),
            ("viewerCanDeleteHeadRef", json!(false)),
        ] {
            details[key] = value;
        }
        let check = json!({
            "name": "build",
            "conclusion": "SUCCESS",
            "isRequired": false,
            "detailsUrl": null,
            "url": "https://github.com/org/app/runs/1",
        });
        let mergeability = json!({
            "mergeable": "MERGEABLE",
            "canBeRebased": true,
            "mergeStateStatus": "CLEAN",
            "baseRef": null,
            "commits": { "pageInfo": { "hasNextPage": false }, "nodes": [{
                "id": "PRC_1",
                "commit": {
                    "id": "C_1",
                    "oid": head,
                    "abbreviatedOid": "abc1234",
                    "status": null,
                    "checkSuites": { "nodes": [{ "checkRuns": { "nodes": [check] } }] },
                },
            }] },
        });
        let commit = json!({
            "id": "C_1",
            "oid": head,
            "abbreviatedOid": "abc1234",
            "url": "",
            "messageHeadline": "Fix the crash",
            "author": null,
        });
        let commit_node = json!({ "id": "PRC_1", "commit": commit, "url": "" });
        let commits = json!({ "commits": { "pageInfo": { "hasNextPage": false }, "nodes": [commit_node] } });
        let viewed = json!({
            "files": {
                "pageInfo": { "hasNextPage": false },
                "nodes": [{ "path": "src/main.rs", "viewerViewedState": "UNVIEWED" }],
            },
        });
        // `gh api --paginate --slurp`: an array of pages.
        let files = json!([[{
            "sha": "s1",
            "filename": "src/main.rs",
            "status": "modified",
            "additions": 2,
            "deletions": 1,
            "changes": 3,
            "patch": "@@ -1 +1,2 @@\n-a\n+b\n+c",
        }]]);
        let repo_view = json!({
            "nameWithOwner": "org/app",
            "url": "https://github.com/org/app",
            "defaultBranchRef": { "name": "main" },
            "mergeCommitAllowed": true,
            "squashMergeAllowed": true,
            "rebaseMergeAllowed": true,
            "viewerPermission": "WRITE",
        });
        let user = json!({
            "login": "me",
            "node_id": "U_me",
            "avatar_url": "",
            "html_url": "https://github.com/me",
            "name": "Me",
        });
        vec![
            ("repo-view.json", repo_view.to_string()),
            ("repository.json", json!({ "data": { "repository": repository() } }).to_string()),
            ("user.json", user.to_string()),
            ("search.json", search(vec![fix.clone(), docs])),
            ("search-crash.json", search(vec![fix])),
            ("pull-request.json", in_pull_request(details)),
            ("mergeability.json", in_pull_request(mergeability)),
            ("commits.json", in_pull_request(commits)),
            ("viewed.json", in_pull_request(viewed)),
            ("files.json", files.to_string()),
            ("checkout-1", "fix-crash".to_owned()),
        ]
    }

    /// gh's calls in the repository at `root`, each its arguments, a tab and the request body.
    fn gh_calls(root: &Path) -> Vec<String> {
        let log = std::fs::read_to_string(root.join(".git").join("fake-gh").join("calls.log"));
        log.unwrap_or_default().lines().map(str::to_owned).collect()
    }

    /// How many of gh's calls in the repository at `root` contain `text`.
    fn calls_with(root: &Path, text: &str) -> usize {
        gh_calls(root).iter().filter(|call| call.contains(text)).count()
    }

    /// How many `findPullRequest` requests gh answered in the repository at `root` (told apart from
    /// the other pull request documents as the fake gh tells them apart).
    fn details_requests(root: &Path) -> usize {
        let others = ["...pullRequestMergeability", "commits(first: ", "files(first: "];
        let calls = gh_calls(root);
        calls.iter().filter(|c| c.contains("pullRequest(number:") && !others.iter().any(|o| c.contains(o))).count()
    }

    /// How many gh calls the window has started.
    fn started(cx: &mut VisualTestContext) -> usize {
        cx.update(|_, cx| cx.global::<GhStandIn>().started.get())
    }

    fn context_on(cx: &mut VisualTestContext, key: &str) -> bool {
        cx.update(|window, cx| window_context(window, cx).is_truthy(key))
    }

    fn context_text(cx: &mut VisualTestContext, key: &str) -> Option<String> {
        cx.update(|window, cx| match window_context(window, cx).get(key) {
            Some(ContextValue::Text(text)) => Some(text.to_string()),
            _ => None,
        })
    }

    fn window_of(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<Entity<PullRequestsWindow>> {
        workbench.read_with(cx, |w, _| w.pull_requests_window().cloned())
    }

    fn content(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Option<ToolWindowContent> {
        let window = window_of(workbench, cx)?;
        Some(window.update(cx, |window, cx| window.content(cx)))
    }

    /// The numbers of the listed pull requests, top to bottom.
    fn listed(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Vec<u32> {
        let Some(window) = window_of(workbench, cx) else { return Vec::new() };
        let list = window.read_with(cx, |window, _| window.list().clone());
        list.read_with(cx, |list, _| list.list().items.iter().map(|pr| pr.number).collect())
    }

    /// View Pull Requests, then the list as gh answers it, once the connection is published.
    fn show_list(
        workbench: &Entity<Workbench>,
        count: usize,
        cx: &mut VisualTestContext,
    ) -> Entity<PullRequestsWindow> {
        run_action(cx, "github.pullRequests", None);
        wait_long(cx, "the pull requests are listed", |cx| {
            listed(workbench, cx).len() == count && context_on(cx, "githubConnected")
        });
        window_of(workbench, cx).expect("the window is open")
    }

    /// View Pull Request on #1, then its details, checks and changes.
    fn open_first(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) -> Entity<DetailsPanel> {
        run_action(cx, "github.pullRequest.show", Some(json!(1)));
        let panel = workbench.read_with(cx, |w, _| w.pull_request_details(1).cloned()).expect("#1 has a tab");
        wait_long(cx, "the pull request loads", |cx| {
            panel.update(cx, |panel, cx| {
                let checks = panel.status_lines(cx).iter().any(|line| line == "All required checks have passed");
                panel.details().is_some() && checks && !panel.tree_rows().is_empty()
            })
        });
        panel
    }

    /// Gives the keyboard to the Pull Requests window (its active tab).
    fn focus_window(workbench: &Entity<Workbench>, cx: &mut VisualTestContext) {
        let view = window_of(workbench, cx).expect("the window is open");
        cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
        cx.run_until_parked();
        assert_eq!(context_text(cx, "activeToolWindow").as_deref(), Some(PULL_REQUESTS_TOOL_WINDOW));
    }

    /// Takes the keyboard away from every view: no tool window is active.
    fn blur(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();
        assert_eq!(context_text(cx, "activeToolWindow"), None);
    }

    fn refresh_keys() -> &'static str {
        if cfg!(target_os = "macos") { "cmd-r" } else { "ctrl-f5" }
    }

    /// View Pull Requests checks gh, connects to the repository of the project's GitHub remote and
    /// lists its open pull requests with their state, author and labels; `githubConnected` is off
    /// until it connects and `githubPrTab` names the list.
    #[gpui_kit::test]
    async fn the_window_connects_and_lists_two_pull_requests_with_their_state_author_and_labels(
        cx: &mut TestAppContext,
    ) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        assert!(!context_on(cx, "githubConnected"), "nothing is connected before the window is shown");
        let window = show_list(&workbench, 2, cx);
        assert_eq!(content(&workbench, cx), Some(ToolWindowContent::Connected));
        assert_eq!(context_text(cx, "githubPrTab").as_deref(), Some("list"));
        let remotes = window.read_with(cx, |window, _| window.repository().remotes.clone());
        let found: Vec<(&str, String)> =
            remotes.iter().map(|r| (r.remote.as_str(), r.coordinates().name_with_owner())).collect();
        assert_eq!(found, [("origin", "org/app".to_owned())], "the remote reads as {GITHUB_REMOTE}");
        let list = window.read_with(cx, |window, _| window.list().clone());
        let rows = list.update(cx, |list, cx| list.rows(cx));
        let shown: Vec<(u32, &str, Option<&str>)> =
            rows.iter().map(|row| (row.number, row.state_icon, row.row.tag)).collect();
        assert_eq!(shown, [(1, "github-pr-open", None), (2, "github-pr-draft", Some("DRAFT"))]);
        assert!(rows[0].row.info.ends_with(", by alice"), "{}", rows[0].row.info);
        assert!(rows[1].row.info.ends_with(", by bob"), "{}", rows[1].row.info);
        let labels: Vec<Vec<String>> = list.read_with(cx, |list, _| {
            list.list().items.iter().map(|pr| pr.labels.nodes.iter().map(|l| l.name.clone()).collect()).collect()
        });
        assert_eq!(labels, [vec!["bug".to_owned()], vec!["docs".to_owned()]]);
        let calls = gh_calls(&root);
        assert!(calls.iter().any(|call| call.starts_with("repo view")), "{calls:?}");
        assert!(calls.iter().any(|c| c.contains("query issueSearch") && c.contains("repo:org/app")), "{calls:?}");
    }

    /// A term typed in the list's search field and applied with Enter asks gh again with it, and
    /// the list shows what that search finds.
    #[gpui_kit::test]
    async fn a_search_term_filters_the_list_through_a_new_request(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        click_on(cx, "gh-search-field");
        cx.simulate_input("crash");
        cx.simulate_keystrokes("enter");
        wait_long(cx, "the search answers", |cx| listed(&workbench, cx) == [1]);
        let searches: Vec<String> =
            gh_calls(&root).into_iter().filter(|call| call.contains("query issueSearch")).collect();
        assert!(searches.len() >= 2, "{searches:?}");
        assert!(!searches[0].contains("(crash)") && searches.last().is_some_and(|s| s.contains("(crash)")));
    }

    /// View Pull Request opens the pull request's tab with its title, its branches, the checks'
    /// summary and the files it changes; `githubPrTab` names a pull request then.
    #[gpui_kit::test]
    async fn view_pull_request_opens_its_tab_with_its_title_branches_checks_and_changed_files(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, _root) = github_project(cx, GITHUB_REMOTE, github_answers);
        let window = show_list(&workbench, 2, cx);
        let panel = open_first(&workbench, cx);
        let (title, base, head) = panel.read_with(cx, |panel, _| {
            let pr = panel.details().expect("the details loaded");
            (pr.short.title.clone(), pr.base_ref_name.clone(), head_branch_label(pr))
        });
        assert_eq!((title.as_str(), base.as_str(), head.as_str()), ("Fix the crash on start", "main", "fix-crash"));
        let files: Vec<Option<String>> =
            panel.read_with(cx, |panel, _| panel.tree_rows().into_iter().map(|row| row.path).collect());
        assert!(files.contains(&Some("src/main.rs".to_owned())), "{files:?}");
        assert_eq!(panel.update(cx, |panel, cx| panel.commit_label(cx)), "Fix the crash");
        let titles = window.update(cx, |window, cx| window.tab_titles(cx));
        assert_eq!(titles.last().map(String::as_str), Some("#1"), "{titles:?}");
        let active = window.read_with(cx, |window, _| window.tabs().active);
        assert_eq!(active, GhTab::PullRequest(1));
        assert_eq!(context_text(cx, "githubPrTab").as_deref(), Some("pr"));
    }

    /// Refresh Pull Request asks gh for the open pull request again.
    #[gpui_kit::test]
    async fn refresh_pull_request_loads_the_open_pull_request_again(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        open_first(&workbench, cx);
        let (before, started_before) = (details_requests(&root), started(cx));
        run_action(cx, "github.pullRequest.refresh", None);
        assert_eq!(started(cx), started_before + 5, "the details, status, commits, changes and viewed states");
        wait_long(cx, "the details are asked for again", |_| details_requests(&root) > before);
    }

    /// Refresh Pull Requests List checks gh and the connection again, then asks for the list again.
    #[gpui_kit::test]
    async fn refresh_list_checks_gh_again_and_asks_for_the_list_again(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        let (views, searches) = (calls_with(&root, "repo view"), calls_with(&root, "query issueSearch"));
        run_action(cx, "github.refresh", None);
        wait_long(cx, "gh is checked and the list asked for again", |_| {
            calls_with(&root, "repo view") > views && calls_with(&root, "query issueSearch") > searches
        });
        wait_long(cx, "the list is shown again", |cx| listed(&workbench, cx) == [1, 2]);
        assert!(context_on(cx, "githubConnected"));
    }

    /// The refresh key refreshes the list with the keyboard in the window's list tab and the open pull
    /// request in its `#N` tab; anywhere else it does nothing.
    #[gpui_kit::test]
    async fn the_refresh_keys_work_only_in_the_pull_requests_window_on_their_tab(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        let window = show_list(&workbench, 2, cx);
        let checking = |cx: &mut VisualTestContext| window.read_with(cx, |window, _| window.repository().checking);

        // The list tab: the list refreshes.
        focus_window(&workbench, cx);
        let searches = calls_with(&root, "query issueSearch");
        cx.simulate_keystrokes(refresh_keys());
        wait_long(cx, "the list is asked for again", |_| calls_with(&root, "query issueSearch") > searches);
        wait_long(cx, "the refresh settles", |cx| !checking(cx) && listed(&workbench, cx).len() == 2);

        // No tool window has the keyboard: nothing happens.
        blur(cx);
        let before = started(cx);
        cx.simulate_keystrokes(refresh_keys());
        cx.run_until_parked();
        assert!(!checking(cx), "the connection is not checked again");
        assert_eq!(started(cx), before, "no gh call starts");

        // The `#1` tab: the pull request loads again (its five requests), the list does not.
        open_first(&workbench, cx);
        focus_window(&workbench, cx);
        assert_eq!(context_text(cx, "githubPrTab").as_deref(), Some("pr"));
        let (before, details) = (started(cx), details_requests(&root));
        cx.simulate_keystrokes(refresh_keys());
        cx.run_until_parked();
        assert!(!checking(cx), "the list's refresh does not run in a pull request's tab");
        assert_eq!(started(cx), before + 5, "the pull request's details, status, commits, changes, viewed");
        wait_long(cx, "the pull request is asked for again", |_| details_requests(&root) == details + 1);

        // The list tab again, with #1 still open: the list refreshes and the pull request does not.
        window.update(cx, |window, cx| window.select_tab(GhTab::List, cx));
        focus_window(&workbench, cx);
        assert_eq!(context_text(cx, "githubPrTab").as_deref(), Some("list"));
        let searches = calls_with(&root, "query issueSearch");
        cx.simulate_keystrokes(refresh_keys());
        wait_long(cx, "the list is asked for again", |_| calls_with(&root, "query issueSearch") > searches);
        wait_long(cx, "the refresh settles", |cx| !checking(cx) && listed(&workbench, cx).len() == 2);
        assert_eq!(details_requests(&root), details + 1, "the pull request's refresh does not run in the list tab");

        // Elsewhere again: nothing happens.
        blur(cx);
        let before = started(cx);
        cx.simulate_keystrokes(refresh_keys());
        cx.run_until_parked();
        assert_eq!(started(cx), before, "no gh call starts");
    }

    /// Checkout Branch on an open pull request runs `gh pr checkout`, says the pull request was
    /// checked out, and the branch widget of the title bar follows to the pull request's branch.
    #[gpui_kit::test]
    async fn checkout_branch_checks_out_the_pull_requests_branch_and_the_branch_widget_follows(
        cx: &mut TestAppContext,
    ) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        open_first(&workbench, cx);
        assert_eq!(branch_label(&workbench, cx), "main");
        run_action(cx, "github.pullRequest.checkoutBranch", None);
        wait_for_notice(&workbench, "Checked out pull request #1", cx);
        wait_long(cx, "the branch widget follows", |cx| branch_label(&workbench, cx) == "fix-crash");
        assert_eq!(git_head(&root), "fix-crash");
        assert!(gh_calls(&root).iter().any(|call| call.starts_with("pr checkout 1")));
    }

    /// Checkout Pull Request… checks out the pull request whose number it is given; a pull request gh
    /// cannot check out says so.
    #[gpui_kit::test]
    async fn checkout_pull_request_checks_out_the_number_it_is_given(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        run_action(cx, "github.checkoutPullRequest", Some(json!(2)));
        wait_for_notice(&workbench, "Couldn't check out #2", cx);
        assert_eq!(git_head(&root), "main");
        run_action(cx, "github.checkoutPullRequest", Some(json!(1)));
        wait_for_notice(&workbench, "Checked out pull request #1", cx);
        assert_eq!(git_head(&root), "fix-crash");
    }

    /// Open on GitHub, View Pull Request in Browser and Copy Pull Request URL use the pull request's
    /// page.
    #[gpui_kit::test]
    async fn the_browser_and_copy_actions_use_the_pull_requests_page(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, _root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        run_action(cx, "github.pullRequest.openInBrowser", Some(json!(2)));
        assert_eq!(cx.opened_url().as_deref(), Some("https://github.com/org/app/pull/2"));
        run_action(cx, "github.pullRequest.copyUrl", Some(json!(2)));
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("https://github.com/org/app/pull/2"));
        open_first(&workbench, cx);
        run_action(cx, "github.openInBrowser", None);
        assert_eq!(cx.opened_url().as_deref(), Some("https://github.com/org/app/pull/1"));
    }

    /// Change Repository or Account… disconnects (`githubConnected` goes off); run again while not
    /// connected, it checks again and connects (`githubConnected` comes back on).
    #[gpui_kit::test]
    async fn change_repository_disconnects_then_connects_again(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, _root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        run_action(cx, "github.changeRepository", None);
        wait_long(cx, "the window disconnects", |cx| !context_on(cx, "githubConnected"));
        assert_ne!(content(&workbench, cx), Some(ToolWindowContent::Connected));
        run_action(cx, "github.changeRepository", None);
        wait_long(cx, "the window connects again", |cx| context_on(cx, "githubConnected"));
        assert_eq!(content(&workbench, cx), Some(ToolWindowContent::Connected));
    }

    /// In Restricted Mode no GitHub command runs, as no other command that runs a program does:
    /// Checkout Pull Request… says it could not check out and leaves the branch, and a refresh
    /// shows why and starts no gh. Trusted again, the window connects and the same checkout runs.
    #[gpui_kit::test]
    async fn in_restricted_mode_the_window_runs_no_gh_until_the_project_is_trusted_again(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        show_list(&workbench, 2, cx);
        run_action(cx, "project.revokeTrust", None);
        wait_long(cx, "the project is restricted", |cx| workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        let (calls, before) = (gh_calls(&root).len(), started(cx));

        run_action(cx, "github.checkoutPullRequest", Some(json!(1)));
        wait_for_notice(&workbench, "Couldn't check out #1", cx);
        let body = workbench.read_with(cx, |w, _| {
            let notice = w.notifications().items().iter().find(|n| n.title == "Couldn't check out #1").cloned();
            notice.and_then(|notice| notice.body)
        });
        assert!(body.is_some_and(|body| body.contains("Restricted Mode")), "the notice says why");
        assert_eq!(git_head(&root), "main");
        run_action(cx, "github.refresh", None);
        let refused = |cx: &mut VisualTestContext| match content(&workbench, cx) {
            Some(ToolWindowContent::Unavailable(shown)) => shown.message.is_some_and(|m| m.contains("Restricted Mode")),
            _ => false,
        };
        wait_long(cx, "the window says why gh does not run", refused);
        assert!(!context_on(cx, "githubConnected"));
        assert_eq!((gh_calls(&root).len(), started(cx)), (calls, before), "no gh ran in Restricted Mode");

        run_action(cx, "project.trust", None);
        wait_long(cx, "the project is trusted", |cx| !workbench.read_with(cx, |w, cx| w.project_restricted(cx)));
        run_action(cx, "github.refresh", None);
        wait_long(cx, "the window connects again", |cx| context_on(cx, "githubConnected"));
        run_action(cx, "github.checkoutPullRequest", Some(json!(1)));
        wait_for_notice(&workbench, "Checked out pull request #1", cx);
        assert_eq!(git_head(&root), "fix-crash");
    }

    /// With gh not signed in, the window says so; Run gh auth login asks for `gh auth login` in a
    /// terminal (recorded by the stand-in: no terminal opens and no gh runs).
    #[gpui_kit::test]
    async fn with_gh_not_signed_in_the_window_shows_the_sign_in_state_and_offers_gh_auth_login(
        cx: &mut TestAppContext,
    ) {
        let message = "To get started with GitHub CLI, please run:  gh auth login\n".to_owned();
        let (workbench, cx, _folders, _root) =
            github_project(cx, GITHUB_REMOTE, move |_| vec![("repo-view.err", message)]);
        run_action(cx, "github.pullRequests", None);
        wait_long(cx, "gh is checked", |cx| matches!(content(&workbench, cx), Some(ToolWindowContent::Unavailable(_))));
        let Some(ToolWindowContent::Unavailable(shown)) = content(&workbench, cx) else {
            panic!("the window explains why it cannot connect");
        };
        assert_eq!(shown.title, "Not logged in to GitHub");
        assert!(shown.login && !shown.install);
        click_on(cx, "gh-login");
        let asked = cx.update(|_, cx| cx.global::<GhStandIn>().terminal.borrow().clone());
        assert_eq!(asked, [json!({ "program": "gh", "args": ["auth", "login"], "name": "gh auth login" })]);
        assert!(!workbench.read_with(cx, |w, _| w.is_tool_window_visible(TERMINAL_TOOL_WINDOW)));
    }

    /// Without gh the window says it is not installed and Install gh… opens its download page.
    #[gpui_kit::test]
    async fn without_gh_installed_the_window_offers_to_install_it(cx: &mut TestAppContext) {
        let (workbench, cx, _folders, root) = github_project(cx, GITHUB_REMOTE, github_answers);
        stand_in(GhCli::not_installed(), cx);
        run_action(cx, "github.pullRequests", None);
        wait_long(cx, "gh is checked", |cx| matches!(content(&workbench, cx), Some(ToolWindowContent::Unavailable(_))));
        let Some(ToolWindowContent::Unavailable(shown)) = content(&workbench, cx) else {
            panic!("the window explains why it cannot connect");
        };
        assert_eq!(shown.title, "GitHub CLI is not installed");
        assert!(shown.install && !shown.login);
        click_on(cx, "gh-install");
        assert_eq!(cx.opened_url().as_deref(), Some(GH_INSTALL_URL));
        assert!(gh_calls(&root).is_empty(), "nothing ran");
    }

    /// A project whose remotes are not on GitHub shows that there is no GitHub repository.
    #[gpui_kit::test]
    async fn without_a_github_remote_the_window_shows_that_state(cx: &mut TestAppContext) {
        let message = "none of the git remotes configured for this repository point to a known GitHub host\n";
        let (workbench, cx, _folders, _root) =
            github_project(cx, "https://gitlab.com/org/app.git", move |_| vec![("repo-view.err", message.to_owned())]);
        run_action(cx, "github.pullRequests", None);
        wait_long(cx, "gh is checked", |cx| matches!(content(&workbench, cx), Some(ToolWindowContent::Unavailable(_))));
        let Some(ToolWindowContent::Unavailable(shown)) = content(&workbench, cx) else {
            panic!("the window explains why it cannot connect");
        };
        assert_eq!(shown.title, "No GitHub repository");
        assert!(!shown.login && !shown.install);
        let repo = workbench.read_with(cx, |w, cx| w.pull_requests_window().map(|v| v.read(cx).repository().clone()));
        assert!(repo.is_some_and(|repo| repo.remotes.is_empty() && repo.connection.is_none()));
    }

    /// A remote URL that carries a token connects as any other, and the token is in none of what the
    /// window keeps (formatted with `{:?}`), shows, copies or notifies.
    #[gpui_kit::test]
    async fn a_token_in_the_remote_url_is_never_kept_shown_copied_or_notified(cx: &mut TestAppContext) {
        let remote = format!("https://ada:{SENTINEL}@github.com/org/app.git");
        let (workbench, cx, _folders, _root) = github_project(cx, &remote, github_answers);
        let window = show_list(&workbench, 2, cx);
        run_action(cx, "github.pullRequest.copyUrl", Some(json!(1)));
        open_first(&workbench, cx);
        let kept = workbench.read_with(cx, |w, cx| {
            let window = w.pull_requests_window().map(|view| view.read(cx));
            let shown = window.map(|view| (view.repository(), view.selector_options(), view.tabs()));
            format!("{:?} {shown:?}", w.github_repository())
        });
        assert!(kept.contains("https://ada:***@github.com/org/app.git"), "the remote is kept masked: {kept}");
        let painted = window.update(cx, |window, cx| format!("{:?} {:?}", window.content(cx), window.tab_titles(cx)));
        let notices = workbench.read_with(cx, |w, _| format!("{:?}", w.notifications().items()));
        let copied = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
        for (what, text) in [("kept", &kept), ("shown", &painted), ("notified", &notices), ("copied", &copied)] {
            assert!(!text.contains(SENTINEL), "the token is {what}: {text}");
        }
        assert!(notification_titles(&workbench, cx).iter().all(|title| !title.contains(SENTINEL)));
    }

    #[test]
    fn remotes_keep_no_credentials_and_still_name_their_repository() {
        let remotes = vec![
            Remote { name: "origin".to_owned(), url: format!("https://ada:{SENTINEL}@github.com/org/app.git") },
            Remote { name: "fork".to_owned(), url: format!("https://{SENTINEL}@github.com/ada/app.git") },
            Remote { name: "ssh".to_owned(), url: "git@github.com:org/app.git".to_owned() },
        ];
        let kept = without_credentials(remotes);
        assert!(!format!("{kept:?}").contains(SENTINEL), "{kept:?}");
        assert_eq!(kept[0].url, "https://ada:***@github.com/org/app.git");
        assert_eq!(kept[1].url, "https://***@github.com/ada/app.git");
        assert_eq!(kept[2].url, "git@github.com:org/app.git");
        let found = crate::github::repository::github_remotes(&kept, None);
        let names: Vec<String> = found.iter().map(|remote| remote.coordinates().name_with_owner()).collect();
        assert_eq!(names, ["org/app", "ada/app"]);
        let lines = [
            format!("fatal: unable to access 'https://ada:{SENTINEL}@github.com/org/app.git/': 403"),
            format!("From https://{SENTINEL}@github.com/org/app\n * branch refs/pull/1/head -> FETCH_HEAD"),
        ];
        for line in lines {
            let masked = redacted_text(&line);
            assert!(!masked.contains(SENTINEL) && masked.contains("github.com/org/app"), "{masked}");
        }
        assert_eq!(redacted_text("no URL here\nnor here"), "no URL here\nnor here");
    }
}
