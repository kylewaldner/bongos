//! Fills the seams for one open project repository: a [`ProjectSource`] serving its loaded
//! definitions joined with its state, and a [`DefinitionWriter`] that routes the changes of
//! commands on its tasks. State-only changes are applied at once in one state transaction;
//! changes to definitions become quick edits: a branch made from the commit the command read,
//! their state ops staged, and a landing that runs on in the background.

use std::sync::Arc;

use bongos_project_repo::router::{self, Routed};
use bongos_project_repo::{
    ApprovalRow, BACKUP_TICK, LandingEvent, LoadedProject, PersonClaim, ProjectRepo,
    ProjectRepositorySettings, QuickEdit, Recovery, RepoCommand, RepoReply, StateEvent,
    check_fresh, written_tasks,
};
use bongos_project_repo_model::ChangeMeta;
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::{Change, Context};
use bongos_work_model::store::{Project, Task as WorkTask, WorkTree};
use chrono::Utc;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor, Task};
use settings::Settings as _;
use util::ResultExt as _;

use crate::ref_watch::{REF_WATCH_LATENCY, RefDirs};
use crate::seams::{DefinitionWriter, DefinitionWriters, ProjectSource, ProjectSources};
use crate::{WorkStore, WriteError, WriteOutcome};

/// The project repositories open in this process, which the commands on working copies, the
/// handling of master files and the readers of run state on other threads serve. Cloning shares
/// the list.
#[derive(Clone, Default)]
pub struct OpenProjectRepos(Arc<parking_lot::Mutex<Vec<Arc<LoadedProject>>>>);

impl gpui::Global for OpenProjectRepos {}

impl OpenProjectRepos {
    /// The app's list, shared: it sees every repository registered later.
    pub fn global(cx: &mut App) -> Self {
        cx.default_global::<Self>().clone()
    }

    /// [`Self::list`] of the app's list, for a caller that cannot create it.
    pub fn listed(cx: &App) -> Vec<Arc<LoadedProject>> {
        open_repos(cx)
    }

    /// Every project repository registered with [`register_project_repo`], now.
    pub fn list(&self) -> Vec<Arc<LoadedProject>> {
        self.0.lock().clone()
    }
}

/// Every project repository registered with [`register_project_repo`].
pub(crate) fn open_repos(cx: &App) -> Vec<Arc<LoadedProject>> {
    cx.try_global::<OpenProjectRepos>()
        .map(OpenProjectRepos::list)
        .unwrap_or_default()
}

/// The open project repository a new-format task belongs to: the one whose files folder holds the
/// task's directory. `None` for an old-format task, a one-off of the old layout, and a task of a
/// repository that is not open.
pub fn project_repo_of(task: &WorkTask, cx: &App) -> Option<ProjectRepo> {
    task.definition.as_ref()?;
    open_repos(cx)
        .into_iter()
        .map(|loaded| loaded.repo().clone())
        .find(|repo| task.dir.starts_with(&repo.location().files))
}

/// The warning of each open project repository whose master was changed outside landing (an
/// outside commit waiting for approval, a split master), by repository id. The project's row in
/// the Work view shows it as a chip.
#[derive(Default)]
struct MasterWarnings(std::collections::HashMap<String, String>);

impl gpui::Global for MasterWarnings {}

/// Records the master warning of the repository `repo_id` (`None` once master is clean again)
/// and redraws the row chips when it changed.
pub(crate) fn note_master_warning(repo_id: &str, warning: Option<String>, cx: &mut App) {
    let warnings = &mut cx.default_global::<MasterWarnings>().0;
    let changed = match warning {
        Some(warning) => {
            warnings
                .insert(repo_id.to_string(), warning.clone())
                .as_ref()
                != Some(&warning)
        }
        None => warnings.remove(repo_id).is_some(),
    };
    if changed {
        crate::registry::RowChipsRegistry::changed(cx);
    }
}

/// The master warning of the open repository `task` belongs to, if it has one.
pub(crate) fn master_warning_of(task: &WorkTask, cx: &App) -> Option<String> {
    let warnings = cx.try_global::<MasterWarnings>()?;
    if warnings.0.is_empty() {
        return None;
    }
    let repo = project_repo_of(task, cx)?;
    warnings.0.get(&repo.location().id).cloned()
}

/// Makes `loaded` known to the commands on working copies and to the handling of master files.
pub(crate) fn note_open_repo(loaded: &Arc<LoadedProject>, cx: &mut App) {
    let repos = OpenProjectRepos::global(cx);
    let mut list = repos.0.lock();
    list.retain(|known| known.repo().location().id != loaded.repo().location().id);
    list.push(loaded.clone());
}

/// `project: approve` of the change `change_id` in whichever open repository holds it, by the
/// person whose context `ctx` is (refused unless it still holds the attestation of a person at
/// a window). Resolves with the approved branch tip.
pub fn approve_proposal(change_id: &str, ctx: Context, cx: &App) -> Task<Result<String, String>> {
    let approving = approving(change_id, ctx, cx);
    cx.background_spawn(async move { approving.await.map(|approved| approved.commit) })
}

/// [`approve_proposal`] for a caller that follows the landing with `watch`: the approval waits
/// the landing announced before the approval was written are passed over (see
/// [`LandingWatch::pass_waits`]); a wait announced after it, and a master-split wait, still hold.
/// Resolves with the watch.
pub fn approve_and_watch(
    change_id: &str,
    ctx: Context,
    mut watch: LandingWatch,
    cx: &App,
) -> Task<Result<LandingWatch, String>> {
    let approving = approving(change_id, ctx, cx);
    let change_id = change_id.to_string();
    cx.background_spawn(async move {
        let approved = approving.await?;
        watch.pass_waits(&change_id, approved.rows);
        Ok(watch)
    })
}

/// An approval written: the branch tip it approved, and how many approval rows the change had
/// once it was written (its own included).
struct Approved {
    commit: String,
    rows: usize,
}

fn approving(
    change_id: &str,
    ctx: Context,
    cx: &App,
) -> impl std::future::Future<Output = Result<Approved, String>> + Send + 'static {
    let found = open_repos(cx)
        .into_iter()
        .map(|loaded| loaded.repo().clone())
        .find(|repo| repo.change(change_id).ok().flatten().is_some());
    let change_id = change_id.to_string();
    async move {
        let repo = found.ok_or_else(|| format!("there is no change {change_id}"))?;
        let approval = repo
            .approve(&change_id, ctx, Utc::now())
            .await
            .map_err(|error| format!("{error:#}"))?;
        // Rows are only ever added, so the position of this one is how many there were once it
        // was written; a later row of another approval does not count.
        let rows = repo
            .approvals(&change_id)
            .map_err(|error| format!("{error:#}"))?;
        let rows = rows
            .iter()
            .rposition(|row| same_approval(row, &approval))
            .map_or(rows.len(), |index| index + 1);
        Ok(Approved {
            commit: approval.commit,
            rows,
        })
    }
}

/// Whether `stored` is the row `written` was stored as (its time keeps only whole seconds).
fn same_approval(stored: &ApprovalRow, written: &ApprovalRow) -> bool {
    stored.commit == written.commit
        && stored.name == written.name
        && stored.basis == written.basis
        && stored.author_patch_id == written.author_patch_id
        && stored.amend_patch_id == written.amend_patch_id
        && stored.at.timestamp() == written.at.timestamp()
}

/// Abandons every open working copy of proposal `number` of the task `root` (its branch and
/// files go, master is untouched). Resolves with how many it abandoned.
pub fn abandon_proposal(root: &str, number: u32, cx: &App) -> Task<Result<usize, String>> {
    let repos: Vec<ProjectRepo> = open_repos(cx)
        .into_iter()
        .map(|loaded| loaded.repo().clone())
        .collect();
    let proposal = (root.to_string(), number);
    cx.background_spawn(async move {
        let mut abandoned = 0;
        for repo in repos {
            let rows = repo
                .changes(&Default::default())
                .map_err(|error| format!("{error:#}"))?;
            for row in rows
                .into_iter()
                .filter(|row| row.author.proposal.as_ref() == Some(&proposal))
            {
                repo.abandon_now(&row.change_id, Utc::now())
                    .await
                    .map_err(|error| error.to_string())?;
                abandoned += 1;
            }
        }
        Ok(abandoned)
    })
}

/// How a landing ended, or where it waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingVerdict {
    Landed,
    /// Refused, in conflict, or waiting for a person; the reason.
    Held(String),
    /// The landing is over without landing and waits for nothing (the change was abandoned, or
    /// the landing failed); the reason.
    Ended(String),
}

/// The landing events of every open repository from the moment it was made, so a landing that
/// starts with the next write is never missed.
pub struct LandingWatch {
    receivers: Vec<futures::channel::mpsc::UnboundedReceiver<(String, LandingEvent)>>,
    /// Events already taken from the receivers by [`LandingWatch::pass_waits`].
    early: Vec<(String, LandingEvent)>,
}

/// Starts watching; call before the write whose landing is to be followed.
pub fn watch_landings(cx: &App) -> LandingWatch {
    LandingWatch {
        receivers: open_repos(cx)
            .iter()
            .map(|loaded| loaded.repo().landing_events())
            .collect(),
        early: Vec::new(),
    }
}

/// Whether `event` is an approval wait of `change_id` announced before the approval that made
/// the change's approval rows number `rows` was written: one that saw fewer rows.
pub(crate) fn is_passed_wait(event: &(String, LandingEvent), change_id: &str, rows: usize) -> bool {
    event.0 == change_id
        && matches!(&event.1, LandingEvent::Waiting { reason, approvals }
            if reason.starts_with("waiting for approval") && *approvals < rows)
}

impl LandingWatch {
    /// Passes over the approval waits of `change_id` announced before the approval that made its
    /// approval rows number `rows` was written (each wait carries the rows the landing had read):
    /// for a caller that has just approved the change, those waits are over. Every other event
    /// counts: the master-split wait, which an approval does not resolve, and a wait announced
    /// after the approval (rows changed but not enough: a second approver, a stale tip).
    pub fn pass_waits(&mut self, change_id: &str, rows: usize) {
        for receiver in &mut self.receivers {
            while let Ok(event) = receiver.try_recv() {
                if !is_passed_wait(&event, change_id, rows) {
                    self.early.push(event);
                }
            }
        }
    }

    /// Resolves when `change_id` landed, is held (refused, conflicting, or waiting for a person
    /// or for master) or its landing ended otherwise (abandoned, failed).
    pub async fn verdict(self, change_id: &str) -> LandingVerdict {
        let early = futures::stream::iter(self.early);
        let mut events = early.chain(futures::stream::select_all(self.receivers));
        while let Some((id, event)) = events.next().await {
            if id != change_id {
                continue;
            }
            match event {
                LandingEvent::Landed { .. } => return LandingVerdict::Landed,
                LandingEvent::Refused { reasons } => {
                    return LandingVerdict::Held(reasons.join("; "));
                }
                LandingEvent::Conflict { files } => {
                    return LandingVerdict::Held(format!("conflict in {}", files.join(", ")));
                }
                LandingEvent::Waiting { reason, .. } => return LandingVerdict::Held(reason),
                LandingEvent::Ended { reason } => return LandingVerdict::Ended(reason),
                LandingEvent::Queued { .. } | LandingEvent::Checking => {}
            }
        }
        LandingVerdict::Held("the repository was closed".to_string())
    }
}

/// Why a write is refused in a process that neither holds the writer nor reaches it.
const NOT_THE_WRITER: &str =
    "this project is open in another bongos process; work there or close it";

fn seam_name(loaded: &LoadedProject) -> String {
    format!("project-repository:{}", loaded.repo().location().id)
}

/// Serves the definitions `loaded` has already loaded in the Work view at once, so a write queued
/// next sees its project; [`register_project_repo`] later replaces the source with its own.
pub(crate) fn serve_now(loaded: &Arc<LoadedProject>, cx: &mut App) {
    ProjectSources::register(
        Arc::new(RepoSource {
            name: seam_name(loaded),
            loaded: loaded.clone(),
        }),
        cx,
    );
}

struct RepoSource {
    name: String,
    loaded: Arc<LoadedProject>,
}

impl ProjectSource for RepoSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn projects(&self) -> Vec<Project> {
        self.loaded.projects().log_err().unwrap_or_default()
    }
}

struct RepoWriter {
    name: String,
    loaded: Arc<LoadedProject>,
    executor: BackgroundExecutor,
}

impl DefinitionWriter for RepoWriter {
    fn name(&self) -> &str {
        &self.name
    }

    fn owns(&self, tree: &WorkTree, change: &Change) -> bool {
        router::owns(tree, &self.loaded.repo().location().id, change)
    }

    fn write(
        &self,
        tree: &WorkTree,
        ctx: &Context,
        attestation: Option<PersonAtKeyboard>,
        change: Change,
    ) -> Result<WriteOutcome, WriteError> {
        match router::route(tree, &change).map_err(WriteError::Refused)? {
            Routed::StateOnly { state, files } => {
                let repo = self.loaded.repo();
                let holds_writer = repo.state().and_then(|state| state.writer()).is_some();
                if !holds_writer && repo.sink().is_none() {
                    return Err(WriteError::Refused(NOT_THE_WRITER.to_string()));
                }
                // Through the project's sink when one is set (the writer of another process),
                // else on this process's writer.
                futures::executor::block_on(repo.apply_state_change(
                    state,
                    &files,
                    ctx.author.clone(),
                ))
                .map_err(|error| WriteError::Failed(format!("{error:#}")))?;
                Ok(WriteOutcome::Applied(change))
            }
            Routed::Definition {
                definition,
                state,
                files,
            } => {
                let (commit, definitions) = self.loaded.shown();
                let Some(base) = commit else {
                    return Err(WriteError::Refused(
                        "the project is still loading; try again".to_string(),
                    ));
                };
                check_fresh(tree, &change, &definitions).map_err(WriteError::Refused)?;
                // A proposal's working copy is named for it, and nobody's marker approves it:
                // the person approves it afterwards with `project: approve`, an agent's waits
                // for the approval hooks.
                let proposal = ctx.author.proposal.as_ref();
                let name = proposal.and_then(|(root, number)| {
                    Some(format!("proposal-{}-{number}", tree.task_by_id(root)?.slug))
                });
                let attestation = attestation.filter(|_| proposal.is_none());
                let edit = QuickEdit {
                    base: base.0,
                    definition,
                    state,
                    files,
                    meta: ChangeMeta::new(String::new(), ctx.author.clone()),
                    tasks: written_tasks(&change),
                    name,
                };
                let repo = self.loaded.repo().clone();
                if repo.state().and_then(|state| state.writer()).is_none() {
                    // Another process holds the writer: the edit travels to it with only a
                    // claim of the person, which it trusts from a verified window alone.
                    if repo.sink().is_none() {
                        return Err(WriteError::Refused(NOT_THE_WRITER.to_string()));
                    }
                    let person = attestation.map(|_| PersonClaim {
                        name: ctx.author.name.clone(),
                    });
                    let reply = futures::executor::block_on(
                        repo.send_command(RepoCommand::QuickEdit { edit, person }),
                    )
                    .map_err(|error| WriteError::Failed(format!("{error:#}")))?;
                    return match reply {
                        RepoReply::Started { change_id } => {
                            Ok(WriteOutcome::Landing { change, change_id })
                        }
                        other => Err(WriteError::Failed(format!(
                            "unexpected answer to a quick edit: {other:?}"
                        ))),
                    };
                }
                // The store's serial writer runs on a background thread and waits for the
                // branch and the row, so the snapshot it publishes next shows the change pending.
                let started =
                    futures::executor::block_on(repo.quick_edit(edit, attestation, Utc::now()))
                        .map_err(|error| WriteError::Failed(format!("{error:#}")))?;
                let change_id = started.change_id;
                let landing_id = change_id.clone();
                self.executor
                    .spawn(async move {
                        if let Err(error) = repo.land_quick_edit(&landing_id, Utc::now()).await {
                            log::error!("landing the quick edit {landing_id}: {error:#}");
                        }
                    })
                    .detach();
                Ok(WriteOutcome::Landing { change, change_id })
            }
        }
    }
}

/// Serves `loaded` in the Work view and routes writes to its tasks. First a landing a crash
/// interrupted is completed, then its definitions are loaded; then every committed state
/// transaction that names a task or a namespace in the tree refreshes the store (a write to
/// `checks.live` alone does not), after loading the definitions again when the verified commit
/// moved, and every change of the repository's refs compares master with the verified commit.
/// The returned task does that until dropped.
pub fn register_project_repo(loaded: Arc<LoadedProject>, cx: &mut App) -> Task<()> {
    register_project_repo_following(loaded, None, cx)
}

/// [`register_project_repo`] for a repository whose changes come from `changes` (another
/// process's writer reports them) when this process does not hold the writer.
pub fn register_project_repo_following(
    loaded: Arc<LoadedProject>,
    changes: Option<futures::channel::mpsc::UnboundedReceiver<StateEvent>>,
    cx: &mut App,
) -> Task<()> {
    let name = seam_name(&loaded);
    note_open_repo(&loaded, cx);
    let holds_writer = loaded
        .repo()
        .state()
        .and_then(|state| state.writer())
        .is_some();
    let events = loaded
        .repo()
        .state()
        .and_then(|state| state.writer())
        .map(|writer| writer.subscribe())
        .or(changes);
    loaded.repo().use_registered_checks(cx);
    DefinitionWriters::register(
        Arc::new(RepoWriter {
            name: name.clone(),
            loaded: loaded.clone(),
            executor: cx.background_executor().clone(),
        }),
        cx,
    );
    crate::registry::register_pending_chips(cx);
    crate::registry::register_missing_chips(cx);
    crate::registry::register_master_chips(cx);
    let source = Arc::new(RepoSource {
        name,
        loaded: loaded.clone(),
    });
    let fs = WorkStore::global(cx).map(|store| store.read(cx).fs());
    cx.spawn(async move |cx| {
        let first = loaded.clone();
        let is_writer = holds_writer;
        cx.background_spawn(async move {
            if is_writer {
                match first.repo().recover_landing().await {
                    Ok(Recovery::Nothing) => {}
                    Ok(recovery) => log::info!("project repository recovery: {recovery:?}"),
                    Err(error) => log::error!("recovering a landing: {error:#}"),
                }
            }
            let loaded = first.load().await;
            first.refresh_marks().log_err();
            loaded
        })
        .await
        .log_err();
        cx.update(|cx| ProjectSources::register(source, cx));
        if is_writer {
            // A move a restart interrupted goes on once both of its projects are open.
            let repos: Vec<ProjectRepo> = cx.update(|cx| {
                open_repos(cx)
                    .iter()
                    .map(|loaded| loaded.repo().clone())
                    .collect()
            });
            drop(crate::move_commands::on_own_thread(move || {
                futures::executor::block_on(crate::move_commands::resume_moves(&repos, Utc::now()));
            }));
        }
        let Some(mut events) = events else {
            return;
        };
        let refresh = async {
            while let Some(StateEvent::Changed {
                task_ids,
                namespaces,
                ..
            }) = events.next().await
            {
                // A write to a namespace outside the tree (`checks.live`) reads nothing again.
                if !crate::store::state_change_refreshes_tree(&task_ids, &namespaces) {
                    continue;
                }
                let reload = loaded.clone();
                cx.background_spawn(async move {
                    let loaded = reload.load().await;
                    reload.refresh_marks().log_err();
                    loaded
                })
                .await
                .log_err();
                cx.update(|cx| {
                    if let Some(store) = WorkStore::global(cx) {
                        store.update(cx, |store, cx| store.refresh_sources(cx));
                    }
                });
            }
        };
        let watch = async {
            if let Some(fs) = fs {
                watch_master(loaded.repo().clone(), fs, cx).await;
            }
        };
        let backups = run_backups(loaded.repo().clone(), cx);
        futures::join!(refresh, watch, backups);
    })
}

/// Stops the backups of a repository when the task that runs its timer is dropped: when the
/// host shuts down, before its state writer (and with it `host.lock`) is dropped.
struct StopBackups(ProjectRepo);

impl Drop for StopBackups {
    fn drop(&mut self) {
        self.0.stop_backups();
    }
}

/// The backup timer of the host: every [`BACKUP_TICK`] it reads the backup settings and asks the
/// repository whether a snapshot, a push, thinning or a copy of run records is due. The tick runs
/// on the background executor, and the state store is held only while the snapshot is read.
async fn run_backups(repo: ProjectRepo, cx: &AsyncApp) {
    let _stop = StopBackups(repo.clone());
    let background = cx.background_executor().clone();
    loop {
        background.timer(BACKUP_TICK).await;
        if repo.backups_stopped() {
            // A writer released to an engine host may come back (a take-back): the timer waits
            // for it rather than ending.
            if repo.backups_released() {
                continue;
            }
            return;
        }
        cx.update(|cx| {
            repo.set_backup_options(ProjectRepositorySettings::get_global(cx).backup_options())
        });
        let ticking = repo.clone();
        background
            .spawn(async move { ticking.backup_tick(Utc::now()).await })
            .await;
    }
}

/// Watches the refs of the repository's `.git` and compares master with the verified commit
/// after every batch that touches one. What it finds is recorded in the state store (an outside
/// commit with its checks, a split master), which refreshes the views.
async fn watch_master(repo: ProjectRepo, fs: Arc<dyn Fs>, cx: &AsyncApp) {
    let git_dir = repo.location().repository.join(".git");
    let background = cx.background_executor().clone();
    let (mut batches, mut dirs) = background
        .spawn({
            let fs = fs.clone();
            async move {
                let (batches, watcher) = fs.watch(&git_dir, REF_WATCH_LATENCY).await;
                (batches, RefDirs::new(git_dir, watcher))
            }
        })
        .await;
    check_master(&repo, cx).await;
    while let Some(batch) = batches.next().await {
        let (matters, returned) = background
            .spawn(async move {
                let matters = dirs.handle(&batch);
                (matters, dirs)
            })
            .await;
        dirs = returned;
        if matters {
            check_master(&repo, cx).await;
        }
    }
}

/// Compares master with the verified commit; a warning (an outside commit, a split master) is
/// logged and shown on the project's row until master is clean again.
async fn check_master(repo: &ProjectRepo, cx: &AsyncApp) {
    let checking = repo.clone();
    let status = cx
        .background_executor()
        .spawn(async move { checking.check_master().await })
        .await;
    match status {
        Ok(status) => {
            let warning = status.warning();
            if let Some(warning) = &warning {
                log::warn!("{warning}");
            }
            let id = repo.location().id.clone();
            cx.update(|cx| note_master_warning(&id, warning, cx));
        }
        Err(error) => log::error!("comparing master with the verified commit: {error:#}"),
    }
}
