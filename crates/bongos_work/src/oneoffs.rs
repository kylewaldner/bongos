//! `task: new one-off` while project repositories are on: one-offs go in the project named by
//! `project_repository.oneoffs_project`, each activity a top-level group of it and each one-off a
//! child of its activity's group. The first one-off makes the project, through the same create
//! path as `project: new`; later ones, and one-offs asked for at the same time, use that project.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::Context as _;
use bongos_project_repo::{
    CreateRequest, Git, GitError, LoadedProject, Location, ProjectRepo, ProjectRepositorySettings,
    RepositoryLocations, StateHandle, StateStore,
};
use bongos_work_model::commands::{self as model, NewTask};
use bongos_work_model::layout;
use bongos_work_model::store::{Project, WorkTree};
use gpui::{App, AppContext as _, AsyncApp, Context, Global, Task, Window};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::Workspace;

use crate::commands::{ready, show_toast, submit};
use crate::project_repos::{open_repos, serve_now};
use crate::projects::{create_request, locations_path};
use crate::register_project_repo;

/// Held while the one-offs project is looked up or made, so two one-offs asked for at once make
/// one project.
static ENSURING: LazyLock<futures::lock::Mutex<()>> =
    LazyLock::new(|| futures::lock::Mutex::new(()));

/// The project of `tree` that holds the one-offs: the project repository titled `title`.
pub(crate) fn oneoffs_project<'a>(tree: &'a WorkTree, title: &str) -> Option<&'a Project> {
    tree.projects.iter().find(|project| {
        project.task.definition.is_some()
            && project
                .file
                .as_ref()
                .is_some_and(|file| file.title.trim() == title.trim())
    })
}

/// Why a new one-off is refused while project repositories are on: old-format one-offs in
/// `tree` are not converted yet. Converting them makes the one-offs project, so making it first
/// would leave them unconvertible.
pub(crate) fn old_oneoffs_block(
    tree: &WorkTree,
    locations: Option<&RepositoryLocations>,
) -> Option<String> {
    let has_old = !tree.activities.is_empty();
    let dir = layout::oneoffs_dir(&tree.root);
    let converted = locations.is_some_and(|locations| locations.converted_from(&dir).is_some());
    (has_old && !converted).then(|| {
        format!(
            "the old one-offs in {} are not converted yet; convert them first with \
             project: convert, then make new one-offs",
            dir.display()
        )
    })
}

/// The location of the one-offs project `request` describes: the one the record at
/// `locations_path` already has in `request.folder`, else a project made now. Whether it was made.
pub(crate) async fn ensure_oneoffs_project(
    request: CreateRequest,
    locations_path: &Path,
    find_git: impl FnOnce() -> Result<Git, GitError>,
) -> anyhow::Result<(Location, bool)> {
    let _held = ENSURING.lock().await;
    let mut locations = RepositoryLocations::load(locations_path)?;
    if let Some(known) = locations
        .locations()
        .iter()
        .find(|location| location.folder == request.folder)
    {
        return Ok((known.clone(), false));
    }
    let title = request.title.clone();
    let repo = ProjectRepo::create_with(request, &mut locations, find_git)
        .await
        .with_context(|| format!("making the project {title} for one-offs"))?;
    Ok((repo.location().clone(), true))
}

/// The one-offs projects this process opened, served until it ends.
#[derive(Default)]
struct OpenedOneoffs(Vec<Task<()>>);

impl Global for OpenedOneoffs {}

/// Files a new one-off with `goal` under `activity` in the one-offs project, making and opening
/// the project first when it is not in the tree yet.
pub(crate) fn new_oneoff(
    activity: String,
    new: NewTask,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let locations = RepositoryLocations::load(locations_path(cx)).log_err();
    if let Some(block) = old_oneoffs_block(&tree, locations.as_ref()) {
        show_toast(workspace, format!("New one-off: {block}"), cx);
        return;
    }
    let settings = ProjectRepositorySettings::get_global(cx).clone();
    let title = settings.oneoffs_project.clone();
    if oneoffs_project(&tree, &title).is_some() {
        submit_oneoff(title, activity, new, workspace, window, cx);
        return;
    }
    let request = create_request(&settings, &title);
    let locations_path = locations_path(cx);
    let ensured = cx.background_spawn(async move {
        ensure_oneoffs_project(request, &locations_path, Git::locate).await
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let (location, created) = match ensured.await {
            Ok(ensured) => ensured,
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, format!("New one-off: {error:#}"), cx)
                    })
                    .log_err();
                return;
            }
        };
        let open = workspace
            .update(cx, |_, cx| {
                open_repos(cx)
                    .iter()
                    .any(|loaded| loaded.repo().location().id == location.id)
            })
            .unwrap_or(true);
        if !open {
            let backup = settings.backup_options();
            let loaded = cx
                .background_spawn(async move { load_location(location, backup).await })
                .await;
            let loaded = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_toast(
                                workspace,
                                format!("New one-off: opening the project {title}: {error:#}"),
                                cx,
                            )
                        })
                        .log_err();
                    return;
                }
            };
            workspace.update(cx, |_, cx| serve(loaded, cx)).log_err();
        }
        workspace
            .update_in(cx, |workspace, window, cx| {
                if created {
                    show_toast(
                        workspace,
                        format!("Made the project {title} for one-offs"),
                        cx,
                    );
                }
                submit_oneoff(title, activity, new, workspace, window, cx);
            })
            .log_err();
    })
    .detach();
}

/// Opens the project repository at `location` for this process: its state writer, its definitions.
async fn load_location(
    location: Location,
    backup: bongos_project_repo::BackupOptions,
) -> anyhow::Result<Arc<LoadedProject>> {
    let git = Git::locate()?;
    let writer = StateStore::open_writer(&location)?;
    let repo = ProjectRepo::new(location, git).with_state(StateHandle::Writer(Arc::new(writer)));
    repo.set_backup_options(backup);
    let loaded = LoadedProject::new(repo);
    loaded.load().await?;
    anyhow::Ok(loaded)
}

/// Opens the project repository at `location` and serves it in the Work view, unless this
/// process already has it open. A project made, added or converted while the app runs, and every
/// project of the record at startup, goes through here.
pub(crate) async fn open_location(location: Location, cx: &mut AsyncApp) -> anyhow::Result<()> {
    let open = cx.update(|cx| {
        open_repos(cx)
            .iter()
            .any(|loaded| loaded.repo().location().id == location.id)
    });
    if open {
        return Ok(());
    }
    let backup = cx.update(|cx| ProjectRepositorySettings::get_global(cx).backup_options());
    let loaded = cx
        .background_spawn(async move { load_location(location, backup).await })
        .await?;
    cx.update(|cx| serve(loaded, cx));
    Ok(())
}

/// Opens every project repository of the record at `locations_path`, once at startup while
/// project repositories are on. One that cannot be opened is logged and left out.
pub(crate) async fn open_known_projects(locations_path: PathBuf, cx: &mut AsyncApp) {
    if !cx.update(|cx| ProjectRepositorySettings::get_global(cx).enabled) {
        return;
    }
    let locations = cx
        .background_spawn(async move { RepositoryLocations::load(locations_path) })
        .await
        .log_err();
    for location in locations.iter().flat_map(|locations| locations.locations()) {
        open_location(location.clone(), cx).await.log_err();
    }
}

/// Serves `loaded` in the Work view at once and keeps it registered.
fn serve(loaded: Arc<LoadedProject>, cx: &mut App) {
    serve_now(&loaded, cx);
    let registration = register_project_repo(loaded, cx);
    cx.default_global::<OpenedOneoffs>().0.push(registration);
}

/// Queues the new one-off on the store's writer, against the one-offs project as the tree has it
/// when the write runs.
fn submit_oneoff(
    title: String,
    activity: String,
    new: NewTask,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    submit(workspace, window, cx, move |tree, ctx| {
        let top = oneoffs_project(tree, &title)
            .and_then(|project| project.task.id())
            .ok_or_else(|| format!("the project {title} for one-offs is not loaded; try again"))?;
        model::new_oneoff_in_project(tree, ctx, top, &activity, new.clone())
    });
}
