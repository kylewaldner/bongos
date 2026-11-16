//! The handlers of the six `extensions` actions: the prompts for missing arguments, the trust
//! question and the echo-area results.
//!
//! A field given in the action is never asked for; a missing one opens a minibuffer session. An
//! install resolves the revision first (`git ls-remote` only, nothing written), then asks whether
//! to trust the source unless the URL, subdirectory and commit are on the curated list, and only
//! then hands the install to the [`Installer`] with the answer. No argument skips the question.
//!
//! The decisions ([`install_from_git`], [`install_language`], [`update`], [`rebuild`],
//! [`remove`]) take an [`Asker`] and return a task, so they run without a window; [`run`] wires
//! them to the minibuffer and the echo area.

use std::collections::HashMap;
use std::fs::{OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_extensions_model::{
    InstallRecord, LanguageList, Layout, ListEntry, ReadOutcome, RecordState, RevSpec, SourceUrl,
    validate_id, validate_relative_path,
};
use bongos_minibuffer::{Candidate, Outcome as SessionOutcome, Session, error, message};
use editor::Editor;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{LocalBoxFuture, Shared};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Global, Task, WeakEntity, Window,
};
use workspace::{OpenOptions as OpenFileOptions, OpenVisible, Workspace};

use crate::ActionKind;
use crate::git::resolve;
use crate::installer::{Installed, Installer, RequestKey, Trust};

/// The name the prompts of language extensions are routed by in `minibuffer.enabled_for`.
pub const HOST: &str = "language_extensions";

/// What every trust question ends with.
pub const RUNS_CODE: &str = "An extension runs code: it can download and run a language server program on this machine. Type yes or no.";

/// The warning of "update to latest" on a listed extension.
pub const LATEST_WARNING: &str =
    "Update to the latest upstream commit (not tested with this bongos)";

/// The trust question of an install that is not on the curated list.
pub fn install_question(url: &SourceUrl, commit: &str) -> String {
    format!("Install {url} at {commit}? {RUNS_CODE} ")
}

/// The trust question of an update of an off-list install to a new commit.
pub fn update_question(url: &SourceUrl, from: &str, to: &str) -> String {
    format!("Update {url} from {from} to {to}? {RUNS_CODE} ")
}

/// The trust question of "update to latest" on a listed extension, which leaves its pin.
pub fn latest_question(url: &SourceUrl, from: &str, to: &str) -> String {
    format!("{LATEST_WARNING}? {url} moves from {from} to {to}. {RUNS_CODE} ")
}

/// Asks the trust question. The answer is `Some(true)` for "yes", `Some(false)` for "no" and
/// `None` when the question was cancelled.
pub trait Asker {
    fn ask(&self, question: String, cx: &mut AsyncApp) -> LocalBoxFuture<'static, Option<bool>>;
}

/// Asks with the strict `yes or no` question of the minibuffer, in a workspace's window.
pub struct MinibufferAsker {
    pub workspace: WeakEntity<Workspace>,
    pub window: AnyWindowHandle,
}

impl Asker for MinibufferAsker {
    fn ask(&self, question: String, cx: &mut AsyncApp) -> LocalBoxFuture<'static, Option<bool>> {
        let (sender, receiver) = oneshot::channel();
        let workspace = self.workspace.clone();
        let opened = cx.update_window(self.window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                bongos_minibuffer::strict_yes_or_no(
                    workspace,
                    HOST,
                    question,
                    move |answer, _, _| {
                        sender.send(answer).ok();
                    },
                    window,
                    cx,
                )
            })
        });
        async move {
            match opened {
                Ok(Ok(())) => receiver.await.ok().flatten(),
                _ => None,
            }
        }
        .boxed_local()
    }
}

/// What a command did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Installed(Installed),
    Updated(Installed),
    Rebuilt(Installed),
    /// The update resolved to the commit already installed; nothing ran.
    UpToDate {
        id: String,
        commit: String,
    },
    /// The trust question was answered "no" or cancelled; nothing was cloned or recorded.
    Declined,
    Removed(String),
}

impl Outcome {
    /// The text the echo area shows.
    pub fn message(&self) -> String {
        match self {
            Outcome::Installed(installed) if installed.already_installed => format!(
                "{} is already installed at {}",
                installed.id, installed.commit
            ),
            Outcome::Installed(installed) => {
                format!("Installed {} at {}", installed.id, installed.commit)
            }
            Outcome::Updated(installed) if installed.already_installed => {
                format!("{} is already at {}", installed.id, installed.commit)
            }
            Outcome::Updated(installed) => {
                format!("Updated {} to {}", installed.id, installed.commit)
            }
            Outcome::Rebuilt(installed) => {
                format!("Rebuilt {} at {}", installed.id, installed.commit)
            }
            Outcome::UpToDate { id, commit } => format!("{id} is already at {commit}"),
            Outcome::Declined => "Nothing installed".to_string(),
            Outcome::Removed(id) => format!("Removed {id}"),
        }
    }
}

pub type CommandResult = Result<Outcome, Arc<anyhow::Error>>;

/// The result of a command, shared by every caller of an identical command.
pub type CommandTask = Shared<LocalBoxFuture<'static, CommandResult>>;

fn arc(err: impl Into<anyhow::Error>) -> Arc<anyhow::Error> {
    Arc::new(err.into())
}

fn ready(result: CommandResult) -> CommandTask {
    futures::future::ready(result).boxed_local().shared()
}

/// What identifies a command for joining: a second identical command while the first is still
/// resolving, asking or installing gets the first one's result.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum JoinKey {
    Install(RequestKey),
    Update { id: String, latest: bool },
}

#[derive(Default)]
struct Running {
    next: u64,
    commands: HashMap<JoinKey, (u64, CommandTask)>,
}

impl Global for Running {}

fn join(
    key: JoinKey,
    cx: &mut App,
    start: impl FnOnce(&mut App) -> Task<CommandResult>,
) -> CommandTask {
    if let Some((_, task)) = cx
        .try_global::<Running>()
        .and_then(|running| running.commands.get(&key))
    {
        return task.clone();
    }
    let task: CommandTask = start(cx).boxed_local().shared();
    let running = cx.default_global::<Running>();
    let token = running.next;
    running.next += 1;
    running.commands.insert(key.clone(), (token, task.clone()));
    let done = task.clone();
    cx.spawn(async move |cx| {
        done.await.ok();
        cx.update(|cx| {
            let running = cx.default_global::<Running>();
            if running
                .commands
                .get(&key)
                .is_some_and(|(other, _)| *other == token)
            {
                running.commands.remove(&key);
            }
        });
    })
    .detach();
    task
}

/// Whether another process holds the OS lock at `path`. Creates nothing: a missing lock file is
/// a free lock.
fn lock_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

fn parse_request(
    url: &str,
    rev: Option<&str>,
    path: Option<&str>,
) -> Result<(RequestKey, RevSpec)> {
    let url = SourceUrl::parse(url)?;
    let rev_spec = RevSpec::parse(rev)?;
    let path = path
        .map(|p| p.trim().trim_matches('/').to_string())
        .filter(|p| !p.is_empty());
    if let Some(path) = &path {
        validate_relative_path(path)?;
    }
    let rev = rev
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(String::from);
    Ok((RequestKey { url, path, rev }, rev_spec))
}

/// Installs `url` at `rev` (`None` for the default branch) from the subdirectory `path`. Resolves
/// the revision, asks `asker` the trust question unless the source and commit are listed, then
/// installs. An identical command still running is joined.
pub fn install_from_git(
    installer: &Entity<Installer>,
    url: &str,
    rev: Option<&str>,
    path: Option<&str>,
    asker: Rc<dyn Asker>,
    cx: &mut App,
) -> CommandTask {
    let (key, rev_spec) = match parse_request(url, rev, path) {
        Ok(parsed) => parsed,
        Err(err) => return ready(Err(arc(err))),
    };
    let installer = installer.clone();
    join(JoinKey::Install(key.clone()), cx, move |cx| {
        cx.spawn(async move |cx| run_install(installer, key, rev_spec, asker, cx).await)
    })
}

async fn run_install(
    installer: Entity<Installer>,
    key: RequestKey,
    rev: RevSpec,
    asker: Rc<dyn Asker>,
    cx: &mut AsyncApp,
) -> CommandResult {
    let (git, list, layout) = installer.read_with(cx, |installer, _| {
        (
            installer.git(),
            installer.list().clone(),
            installer.layout().clone(),
        )
    });
    // Another process installing this source: say so before asking anything.
    let lock = layout.lock_path(&layout.source_key(&key.url));
    if cx.background_spawn(async move { lock_held(&lock) }).await {
        return Err(arc(anyhow!("another bongos is installing {}", key.url)));
    }
    let commit = {
        let url = key.url.clone();
        cx.background_spawn(async move { resolve(git.as_ref(), &url, &rev).await })
            .await
            .map_err(arc)?
    };
    let trust = if list.is_listed(&key.url, key.path.as_deref(), &commit) {
        Trust::Listed
    } else {
        match asker.ask(install_question(&key.url, &commit), cx).await {
            Some(true) => Trust::Confirmed(commit),
            Some(false) | None => return Ok(Outcome::Declined),
        }
    };
    let task = installer.update(cx, |installer, cx| {
        installer.install(
            key.url.as_str(),
            key.rev.as_deref(),
            key.path.as_deref(),
            trust,
            cx,
        )
    });
    Ok(Outcome::Installed(task.await?))
}

/// The list entry whose name or id is `name`, ignoring case.
pub fn find_language<'a>(list: &'a LanguageList, name: &str) -> Option<&'a ListEntry> {
    let name = name.trim();
    list.entries()
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name) || entry.id.eq_ignore_ascii_case(name))
}

/// Installs the list entry named `name` (its name or id, ignoring case) at its pinned commit.
/// A listed install at its pin never asks.
pub fn install_language(
    installer: &Entity<Installer>,
    name: &str,
    asker: Rc<dyn Asker>,
    cx: &mut App,
) -> CommandTask {
    let entry = find_language(installer.read(cx).list(), name).cloned();
    let Some(entry) = entry else {
        return ready(Err(arc(anyhow!(
            "no language named `{}` is on the list of language extensions",
            name.trim()
        ))));
    };
    install_from_git(
        installer,
        &entry.repository,
        Some(&entry.commit),
        entry.path.as_deref(),
        asker,
        cx,
    )
}

/// Updates the git-installed `id`. A listed install moves to its pin without asking; with
/// `latest` it moves to the remote `HEAD` and asks, since that commit is off the list. An
/// off-list install refetches its requested ref and asks again, naming both commits, when that
/// resolves to a new commit.
pub fn update(
    installer: &Entity<Installer>,
    id: &str,
    latest: bool,
    asker: Rc<dyn Asker>,
    cx: &mut App,
) -> CommandTask {
    let id = id.trim().to_string();
    if let Err(err) = validate_id(&id) {
        return ready(Err(arc(err)));
    }
    let installer = installer.clone();
    let key = JoinKey::Update {
        id: id.clone(),
        latest,
    };
    join(key, cx, move |cx| {
        cx.spawn(async move |cx| run_update(installer, id, latest, asker, cx).await)
    })
}

fn read_record(layout: &Layout, id: &str) -> Result<InstallRecord> {
    let path = layout.record_path(id);
    match bongos_extensions_model::read(&path)
        .with_context(|| format!("could not read {}", path.display()))?
    {
        ReadOutcome::Ok(record) if record.state == RecordState::Installed => Ok(record),
        ReadOutcome::Ok(_) => bail!("{id} is being removed"),
        ReadOutcome::Missing | ReadOutcome::Unreadable => bail!("{id} is not installed from git"),
    }
}

fn same_path(a: Option<&str>, b: Option<&str>) -> bool {
    let normalize = |path: Option<&str>| {
        path.map(|path| path.trim_matches('/').to_string())
            .filter(|path| !path.is_empty())
    };
    normalize(a) == normalize(b)
}

async fn run_update(
    installer: Entity<Installer>,
    id: String,
    latest: bool,
    asker: Rc<dyn Asker>,
    cx: &mut AsyncApp,
) -> CommandResult {
    let (git, list, layout, busy) = installer.read_with(cx, |installer, _| {
        (
            installer.git(),
            installer.list().clone(),
            installer.layout().clone(),
            installer.busy_message(&id),
        )
    });
    if let Some(busy) = busy {
        return Err(arc(anyhow!(busy)));
    }
    let record = {
        let id = id.clone();
        cx.background_spawn(async move {
            if lock_held(&layout.lock_path(&id)) {
                bail!("{id} is busy: another bongos is changing it");
            }
            read_record(&layout, &id)
        })
        .await
        .map_err(arc)?
    };
    let url = SourceUrl::parse(&record.source_url).map_err(|err| {
        arc(anyhow::Error::from(err).context(format!("the record of {id} is damaged")))
    })?;
    let pin = list
        .entries()
        .iter()
        .find(|entry| {
            entry.id == id
                && SourceUrl::parse(&entry.repository).is_ok_and(|listed| listed == url)
                && same_path(entry.path.as_deref(), record.path.as_deref())
        })
        .map(|entry| entry.commit.clone());
    let (rev_text, to_latest) = match pin {
        Some(pin) if record.listed && !latest => (Some(pin), false),
        _ if record.listed && latest => (None, true),
        _ => (record.requested_ref.clone(), false),
    };
    let rev = RevSpec::parse(rev_text.as_deref()).map_err(arc)?;
    let commit = {
        let url = url.clone();
        cx.background_spawn(async move { resolve(git.as_ref(), &url, &rev).await })
            .await
            .map_err(arc)?
    };
    if commit.eq_ignore_ascii_case(&record.commit) {
        return Ok(Outcome::UpToDate { id, commit });
    }
    let trust = if list.is_listed(&url, record.path.as_deref(), &commit) {
        Trust::Listed
    } else {
        let question = if to_latest {
            latest_question(&url, &record.commit, &commit)
        } else {
            update_question(&url, &record.commit, &commit)
        };
        match asker.ask(question, cx).await {
            Some(true) => Trust::Confirmed(commit),
            Some(false) | None => return Ok(Outcome::Declined),
        }
    };
    let task = installer.update(cx, |installer, cx| {
        installer.update(&id, rev_text.as_deref(), trust, cx)
    });
    Ok(Outcome::Updated(task.await?))
}

/// Rebuilds the git-installed `id` from its current checkout. Same commit, so no question.
pub fn rebuild(installer: &Entity<Installer>, id: &str, cx: &mut App) -> CommandTask {
    let task = installer.update(cx, |installer, cx| installer.rebuild(id.trim(), cx));
    async move { task.await.map(Outcome::Rebuilt) }
        .boxed_local()
        .shared()
}

/// Removes the git-installed `id`.
pub fn remove(installer: &Entity<Installer>, id: &str, cx: &mut App) -> CommandTask {
    let id = id.trim().to_string();
    let task = installer.update(cx, |installer, cx| installer.remove(&id, cx));
    async move { task.await.map(|()| Outcome::Removed(id)).map_err(Arc::new) }
        .boxed_local()
        .shared()
}

/// The build log `extensions: show build log` opens: the log of `id`, or without an id the log of
/// the newest running build, else of the last failed one.
pub fn build_log_path(installer: &Installer, id: Option<&str>) -> Result<PathBuf> {
    let layout = installer.layout();
    if let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) {
        validate_id(id)?;
        return Ok(layout.log_path(id));
    }
    if let Some(operation) = installer.operations().last() {
        let key = match &operation.id {
            Some(id) => id.clone(),
            None => layout.source_key(&operation.key.url),
        };
        return Ok(layout.log_path(&key));
    }
    installer
        .last_failure()
        .and_then(|failure| failure.log_path.clone())
        .ok_or_else(|| anyhow!("no extension build failed or is running"))
}

/// The installs recorded under `git/`, by id.
pub fn git_installs(layout: &Layout) -> Vec<InstallRecord> {
    let Ok(entries) = std::fs::read_dir(layout.git_dir()) else {
        return Vec::new();
    };
    let mut records: Vec<InstallRecord> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|id| validate_id(id).is_ok())
        .filter_map(|id| read_record(layout, &id).ok())
        .collect();
    records.sort_by(|a, b| a.id.cmp(&b.id));
    records
}

fn source_text(repository: &str, path: Option<&str>) -> String {
    match path {
        Some(path) => format!("{repository} ({path})"),
        None => repository.to_string(),
    }
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The candidates of `extensions: install language`: every list entry, with its description and
/// source. The value is the entry's name.
pub fn language_candidates(list: &LanguageList) -> Vec<Candidate> {
    list.entries()
        .iter()
        .map(|entry| {
            Candidate::new(entry.name.clone(), entry.name.clone()).secondary(format!(
                "{} From {}",
                entry.description,
                source_text(&entry.repository, entry.path.as_deref())
            ))
        })
        .collect()
}

/// The candidates of the id pickers: every install from git, with its source and commit. The
/// value is the id.
pub fn install_candidates(records: &[InstallRecord]) -> Vec<Candidate> {
    records
        .iter()
        .map(|record| {
            Candidate::new(record.id.clone(), record.id.clone()).secondary(format!(
                "{} at {}",
                source_text(&record.source_url, record.path.as_deref()),
                short(&record.commit)
            ))
        })
        .collect()
}

/// Runs an action for the workspace `workspace` in `window`: prompts for what is missing, then
/// echoes the result.
pub fn run(kind: ActionKind, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let given = |text: Option<String>| text.filter(|text| !text.trim().is_empty());
    match kind {
        ActionKind::InstallFromGit(action) => match given(action.url) {
            Some(url) => start_install(&workspace, url, action.rev, action.path, window, cx),
            None => read_url(workspace, action.rev, action.path, window, cx),
        },
        ActionKind::InstallLanguage(action) => match given(action.name) {
            Some(name) => start_language(&workspace, name, window, cx),
            None => pick_language(workspace, window, cx),
        },
        ActionKind::Update(action) => {
            let latest = action.latest.unwrap_or(false);
            match given(action.id) {
                Some(id) => start_update(&workspace, id, latest, window, cx),
                None => pick_install(
                    workspace,
                    "Update extension: ",
                    window,
                    cx,
                    move |w, id, window, cx| start_update(&w, id, latest, window, cx),
                ),
            }
        }
        ActionKind::Rebuild(action) => match given(action.id) {
            Some(id) => start_rebuild(id, window, cx),
            None => pick_install(
                workspace,
                "Rebuild extension: ",
                window,
                cx,
                |_, id, window, cx| start_rebuild(id, window, cx),
            ),
        },
        ActionKind::Remove(action) => match given(action.id) {
            Some(id) => start_remove(id, window, cx),
            None => pick_install(
                workspace,
                "Remove extension: ",
                window,
                cx,
                |w, id, window, cx| confirm_remove(w, id, window, cx),
            ),
        },
        ActionKind::ShowBuildLog(action) => show_build_log(workspace, action.id, window, cx),
    }
}

fn asker(workspace: &WeakEntity<Workspace>, window: &Window) -> Rc<dyn Asker> {
    Rc::new(MinibufferAsker {
        workspace: workspace.clone(),
        window: window.window_handle(),
    })
}

fn echo(result: &CommandResult, window: &mut Window, cx: &mut App) {
    match result {
        Ok(outcome) => message(window, cx, outcome.message()),
        Err(err) => error(window, cx, format!("{err:#}")),
    }
}

/// Echoes the result of `task` in `window` once it is done.
fn echo_when_done(task: CommandTask, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            cx.update(|window, cx| echo(&result, window, cx)).ok();
        })
        .detach();
}

/// Opens `session` in the workspace once the current update is over.
fn open_session(
    workspace: WeakEntity<Workspace>,
    session: Session,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        workspace
            .update(cx, |workspace, cx| {
                bongos_minibuffer::open(workspace, session, window, cx);
            })
            .ok();
    });
}

fn chosen(outcome: SessionOutcome) -> Option<String> {
    match outcome {
        SessionOutcome::Candidate(value) => value.downcast_ref::<String>().cloned(),
        SessionOutcome::Input(input) => Some(input),
    }
}

fn start_install(
    workspace: &WeakEntity<Workspace>,
    url: String,
    rev: Option<String>,
    path: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let installer = Installer::global(cx);
    let asker = asker(workspace, window);
    let task = install_from_git(&installer, &url, rev.as_deref(), path.as_deref(), asker, cx);
    echo_when_done(task, window, cx);
}

/// The URL read, then the ref read unless the action gave a ref.
fn read_url(
    workspace: WeakEntity<Workspace>,
    rev: Option<String>,
    path: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let w = workspace.clone();
    let session = Session::string(HOST, "Install from git (repository URL): ")
        .history(format!("{HOST} url"))
        .accept(|input| !input.trim().is_empty(), "Give the repository URL.")
        .on_confirm(move |outcome, window, cx| {
            let Some(url) = chosen(outcome) else { return };
            match rev {
                Some(rev) => start_install(&w, url, Some(rev), path, window, cx),
                None => read_rev(w, url, path, window, cx),
            }
        });
    open_session(workspace, session, window, cx);
}

fn read_rev(
    workspace: WeakEntity<Workspace>,
    url: String,
    path: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let w = workspace.clone();
    let session = Session::string(
        HOST,
        "Branch, tag or commit (empty for the default branch): ",
    )
    .history(format!("{HOST} ref"))
    .on_confirm(move |outcome, window, cx| {
        let rev = chosen(outcome).filter(|rev| !rev.trim().is_empty());
        start_install(&w, url, rev, path, window, cx);
    });
    open_session(workspace, session, window, cx);
}

fn start_language(
    workspace: &WeakEntity<Workspace>,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let installer = Installer::global(cx);
    let asker = asker(workspace, window);
    let task = install_language(&installer, &name, asker, cx);
    echo_when_done(task, window, cx);
}

fn pick_language(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let installer = Installer::global(cx);
    let candidates = language_candidates(installer.read(cx).list());
    let w = workspace.clone();
    let session = Session::completing_read(HOST, "Install language: ")
        .source(bongos_minibuffer::Source::Fixed(candidates))
        .on_confirm(move |outcome, window, cx| {
            if let Some(name) = chosen(outcome) {
                start_language(&w, name, window, cx);
            }
        });
    open_session(workspace, session, window, cx);
}

/// Opens a picker of the installs from git and calls `then` with the chosen id.
fn pick_install(
    workspace: WeakEntity<Workspace>,
    prompt: &'static str,
    window: &mut Window,
    cx: &mut App,
    then: impl FnOnce(WeakEntity<Workspace>, String, &mut Window, &mut App) + 'static,
) {
    let layout = Installer::global(cx).read(cx).layout().clone();
    window
        .spawn(cx, async move |cx| {
            let records = cx
                .background_spawn(async move { git_installs(&layout) })
                .await;
            cx.update(|window, cx| {
                if records.is_empty() {
                    message(window, cx, "No extensions are installed from git");
                    return;
                }
                let w = workspace.clone();
                let session = Session::completing_read(HOST, prompt)
                    .source(bongos_minibuffer::Source::Fixed(install_candidates(
                        &records,
                    )))
                    .on_confirm(move |outcome, window, cx| {
                        if let Some(id) = chosen(outcome) {
                            then(w, id, window, cx);
                        }
                    });
                open_session(workspace, session, window, cx);
            })
            .ok();
        })
        .detach();
}

fn start_update(
    workspace: &WeakEntity<Workspace>,
    id: String,
    latest: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let installer = Installer::global(cx);
    let asker = asker(workspace, window);
    let task = update(&installer, &id, latest, asker, cx);
    echo_when_done(task, window, cx);
}

fn start_rebuild(id: String, window: &mut Window, cx: &mut App) {
    let installer = Installer::global(cx);
    let task = rebuild(&installer, &id, cx);
    echo_when_done(task, window, cx);
}

fn start_remove(id: String, window: &mut Window, cx: &mut App) {
    let installer = Installer::global(cx);
    let task = remove(&installer, &id, cx);
    echo_when_done(task, window, cx);
}

/// The `y`/`n` question after the remove picker.
fn confirm_remove(workspace: WeakEntity<Workspace>, id: String, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        workspace
            .update(cx, |workspace, cx| {
                bongos_minibuffer::yes_or_no(
                    workspace,
                    HOST,
                    format!("Remove {id}? "),
                    false,
                    move |yes, window, cx| {
                        if yes {
                            start_remove(id, window, cx);
                        }
                    },
                    window,
                    cx,
                );
            })
            .ok();
    });
}

fn show_build_log(
    workspace: WeakEntity<Workspace>,
    id: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let installer = Installer::global(cx);
    let path = match build_log_path(installer.read(cx), id.as_deref()) {
        Ok(path) => path,
        Err(err) => {
            error(window, cx, format!("{err:#}"));
            return;
        }
    };
    window
        .spawn(cx, async move |cx| {
            let exists = {
                let path = path.clone();
                cx.background_spawn(async move { path.is_file() }).await
            };
            if !exists {
                cx.update(|window, cx| {
                    error(window, cx, format!("No build log at {}", path.display()))
                })
                .ok();
                return;
            }
            let opened = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    path,
                    OpenFileOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            let Ok(opened) = opened else { return };
            match opened.await {
                Ok(item) => {
                    if let Some(editor) = item.downcast::<Editor>() {
                        cx.update(|_, cx| {
                            editor.update(cx, |editor, _| editor.set_read_only(true))
                        })
                        .ok();
                    }
                }
                Err(err) => {
                    cx.update(|window, cx| error(window, cx, format!("{err:#}")))
                        .ok();
                }
            }
        })
        .detach();
}
