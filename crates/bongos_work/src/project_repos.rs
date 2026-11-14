//! Fills the seams for one open project repository: a [`ProjectSource`] serving its loaded
//! definitions joined with its state, and a [`DefinitionWriter`] that routes the changes of
//! commands on its tasks. State-only changes are applied at once in one state transaction;
//! changes to definitions become quick edits: a branch made from the commit the command read,
//! their state ops staged, and a landing that runs on in the background.

use std::sync::Arc;

use bongos_project_repo::router::{self, Routed};
use bongos_project_repo::{
    LandingEvent, LoadedProject, ProjectRepo, QuickEdit, Recovery, StateEvent, check_fresh,
    written_tasks,
};
use bongos_project_repo_model::ChangeMeta;
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::{Change, Context};
use bongos_work_model::store::{Project, WorkTree};
use chrono::Utc;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor, Task};
use util::ResultExt as _;

use crate::ref_watch::{REF_WATCH_LATENCY, RefDirs};
use crate::seams::{DefinitionWriter, DefinitionWriters, ProjectSource, ProjectSources};
use crate::{WorkStore, WriteError, WriteOutcome};

/// The project repositories open in this process, which the commands on working copies and the
/// handling of master files serve.
#[derive(Default)]
struct OpenRepos(Vec<Arc<LoadedProject>>);

impl gpui::Global for OpenRepos {}

/// Every project repository registered with [`register_project_repo`].
pub(crate) fn open_repos(cx: &App) -> Vec<Arc<LoadedProject>> {
    cx.try_global::<OpenRepos>()
        .map(|repos| repos.0.clone())
        .unwrap_or_default()
}

/// Makes `loaded` known to the commands on working copies and to the handling of master files.
pub(crate) fn note_open_repo(loaded: &Arc<LoadedProject>, cx: &mut App) {
    let repos = cx.default_global::<OpenRepos>();
    repos
        .0
        .retain(|known| known.repo().location().id != loaded.repo().location().id);
    repos.0.push(loaded.clone());
}

/// `project: approve` of the change `change_id` in whichever open repository holds it, by the
/// person whose context `ctx` is (refused unless it still holds the attestation of a person at
/// a window). Resolves with the approved branch tip.
pub fn approve_proposal(change_id: &str, ctx: Context, cx: &App) -> Task<Result<String, String>> {
    let approving = approving(change_id, ctx, cx);
    cx.background_spawn(approving)
}

/// [`approve_proposal`] for a caller that follows the landing with `watch`: the moment the
/// approval is written, the waits the landing announced before it are passed over (see
/// [`LandingWatch::pass_waits`]); a wait announced after it still holds. Resolves with the watch.
pub fn approve_and_watch(
    change_id: &str,
    ctx: Context,
    mut watch: LandingWatch,
    cx: &App,
) -> Task<Result<LandingWatch, String>> {
    let approving = approving(change_id, ctx, cx);
    let change_id = change_id.to_string();
    cx.background_spawn(async move {
        approving.await?;
        watch.pass_waits(&change_id);
        Ok(watch)
    })
}

fn approving(
    change_id: &str,
    ctx: Context,
    cx: &App,
) -> impl std::future::Future<Output = Result<String, String>> + Send + 'static {
    let found = open_repos(cx)
        .into_iter()
        .map(|loaded| loaded.repo().clone())
        .find(|repo| repo.change(change_id).ok().flatten().is_some());
    let change_id = change_id.to_string();
    async move {
        let repo = found.ok_or_else(|| format!("there is no change {change_id}"))?;
        repo.approve(&change_id, ctx, Utc::now())
            .await
            .map(|approval| approval.commit)
            .map_err(|error| format!("{error:#}"))
    }
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

impl LandingWatch {
    /// Passes over every wait of `change_id` announced so far: for a caller that has just
    /// approved the change, a wait the landing announced before the approval is over. Every other
    /// event, and every wait announced from now on, still counts.
    pub fn pass_waits(&mut self, change_id: &str) {
        for receiver in &mut self.receivers {
            while let Ok(Some(event)) = receiver.try_next() {
                let passed =
                    event.0 == change_id && matches!(event.1, LandingEvent::Waiting { .. });
                if !passed {
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
                LandingEvent::Waiting { reason } => return LandingVerdict::Held(reason),
                LandingEvent::Ended { reason } => return LandingVerdict::Ended(reason),
                LandingEvent::Queued { .. } | LandingEvent::Checking => {}
            }
        }
        LandingVerdict::Held("the repository was closed".to_string())
    }
}

fn seam_name(loaded: &LoadedProject) -> String {
    format!("project-repository:{}", loaded.repo().location().id)
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
                let Some(writer) = repo.state().and_then(|state| state.writer()) else {
                    return Err(WriteError::Refused(
                        "this project is open in another bongos process; work there or close it"
                            .to_string(),
                    ));
                };
                router::apply_state_only(
                    writer,
                    &repo.location().files,
                    &state,
                    &files,
                    &ctx.author,
                )
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
/// transaction refreshes the store, after loading the definitions again when the verified commit
/// moved, and every change of the repository's refs compares master with the verified commit.
/// The returned task does that until dropped.
pub fn register_project_repo(loaded: Arc<LoadedProject>, cx: &mut App) -> Task<()> {
    let name = seam_name(&loaded);
    note_open_repo(&loaded, cx);
    let events = loaded
        .repo()
        .state()
        .and_then(|state| state.writer())
        .map(|writer| writer.subscribe());
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
    let source = Arc::new(RepoSource {
        name,
        loaded: loaded.clone(),
    });
    let fs = WorkStore::global(cx).map(|store| store.read(cx).fs());
    cx.spawn(async move |cx| {
        let first = loaded.clone();
        let is_writer = events.is_some();
        cx.background_spawn(async move {
            if is_writer {
                match first.repo().recover_landing().await {
                    Ok(Recovery::Nothing) => {}
                    Ok(recovery) => log::info!("project repository recovery: {recovery:?}"),
                    Err(error) => log::error!("recovering a landing: {error:#}"),
                }
            }
            first.load().await
        })
        .await
        .log_err();
        cx.update(|cx| ProjectSources::register(source, cx));
        let Some(mut events) = events else {
            return;
        };
        let refresh = async {
            while let Some(StateEvent::Changed { .. }) = events.next().await {
                let reload = loaded.clone();
                cx.background_spawn(async move { reload.load().await })
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
        futures::join!(refresh, watch);
    })
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
    check_master(&repo, &background).await;
    while let Some(batch) = batches.next().await {
        let (matters, returned) = background
            .spawn(async move {
                let matters = dirs.handle(&batch);
                (matters, dirs)
            })
            .await;
        dirs = returned;
        if matters {
            check_master(&repo, &background).await;
        }
    }
}

async fn check_master(repo: &ProjectRepo, background: &gpui::BackgroundExecutor) {
    let repo = repo.clone();
    let status = background
        .spawn(async move { repo.check_master().await })
        .await;
    match status {
        Ok(status) => {
            if let Some(warning) = status.warning() {
                log::warn!("{warning}");
            }
        }
        Err(error) => log::error!("comparing master with the verified commit: {error:#}"),
    }
}
