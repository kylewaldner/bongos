//! Fills the seams for one open project repository: a [`ProjectSource`] serving its loaded
//! definitions joined with its state, and a [`DefinitionWriter`] that routes the changes of
//! commands on its tasks. State-only changes are applied at once in one state transaction;
//! changes to definitions become quick edits: a branch made from the commit the command read,
//! their state ops staged, and a landing that runs on in the background.

use std::sync::Arc;

use bongos_project_repo::router::{self, Routed};
use bongos_project_repo::{
    LoadedProject, ProjectRepo, QuickEdit, Recovery, StateEvent, check_fresh, written_tasks,
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
                let edit = QuickEdit {
                    base: base.0,
                    definition,
                    state,
                    files,
                    meta: ChangeMeta::new(String::new(), ctx.author.clone()),
                    tasks: written_tasks(&change),
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
