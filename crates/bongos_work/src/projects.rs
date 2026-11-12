//! The handlers of the `project` actions: `new`, `archive`, `unarchive` and `open folder`.

use bongos_project_repo::{
    CreateRequest, ProjectRepo, ProjectRepositorySettings, RepositoryLocations,
};
use bongos_work_model::commands::{self as model, NewProject};
use bongos_work_model::slug::slugify;
use gpui::{AppContext as _, Context, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::actions::project;
use crate::commands::{ready, show_toast, submit, with_project};
use crate::modals;

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &project::New, window, cx| new_project(w, window, cx))
        .register_action(|w, _: &project::Archive, window, cx| archive(w, true, window, cx))
        .register_action(|w, _: &project::Unarchive, window, cx| archive(w, false, window, cx))
        .register_action(|w, _: &project::OpenFolder, window, cx| open_folder(w, window, cx));
}

fn new_project(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    modals::ask(
        workspace,
        "Name of the new project",
        "",
        false,
        Box::new(|title, workspace, _, cx| {
            if ProjectRepositorySettings::get_global(cx).enabled {
                new_repository_project(title, workspace, cx);
                return;
            }
            submit(workspace, cx, move |tree, ctx| {
                model::project_new(
                    tree,
                    ctx,
                    NewProject {
                        title: title.clone(),
                        ..NewProject::default()
                    },
                )
            })
        }),
        window,
        cx,
    );
}

/// `project: new` while `project_repository.enabled`: makes the project folder with its
/// repository instead of an old-format project.
fn new_repository_project(title: String, workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let settings = ProjectRepositorySettings::get_global(cx).clone();
    let request = CreateRequest {
        folder: settings.project_folder(&slugify(&title)),
        layout: settings.layout,
        title: title.clone(),
        repositories: Vec::new(),
        remote: None,
    };
    let created = cx.background_spawn(async move {
        let mut locations = RepositoryLocations::load(RepositoryLocations::default_path())?;
        ProjectRepo::create(request, &mut locations).await?;
        anyhow::Ok(())
    });
    cx.spawn(async move |workspace, cx| {
        let message = match created.await {
            Ok(()) => format!("Created project {title}"),
            Err(error) => error.to_string(),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}

fn archive(
    workspace: &mut Workspace,
    archive: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (placeholder, none) = if archive {
        ("Archive which project?", "There is no project to archive")
    } else {
        ("Unarchive which project?", "There is no archived project")
    };
    with_project(
        workspace,
        window,
        cx,
        placeholder,
        none,
        move |project| {
            project
                .file
                .as_ref()
                .is_some_and(|file| file.archived != archive)
        },
        Box::new(move |slug, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| {
                if archive {
                    model::project_archive(tree, ctx, &slug)
                } else {
                    model::project_unarchive(tree, ctx, &slug)
                }
            })
        }),
    );
}

fn open_folder(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_project(
        workspace,
        window,
        cx,
        "Open the folder of which project?",
        "There is no project",
        |_| true,
        Box::new(|slug, workspace, _, cx| {
            let Some(dir) = ready(workspace, cx).and_then(|tree| {
                tree.projects
                    .iter()
                    .find(|project| project.slug == slug)
                    .map(|project| project.task.dir.clone())
            }) else {
                return;
            };
            let added = workspace.project().update(cx, |project, cx| {
                project.find_or_create_worktree(dir, true, cx)
            });
            cx.spawn(async move |workspace, cx| {
                if let Err(error) = added.await {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_toast(workspace, format!("Could not open the folder: {error}"), cx)
                        })
                        .ok();
                }
            })
            .detach();
        }),
    );
}
