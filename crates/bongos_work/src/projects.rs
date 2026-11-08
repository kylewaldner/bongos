//! The handlers of the `project` actions: `new` (which also runs `git init` in the new project's
//! directory, so its plans and notes have history), `archive`, `unarchive` and `open folder`.

use bongos_work_model::commands::{self as model, NewProject};
use gpui::{Context, Window};
use workspace::Workspace;

use crate::actions::project;
use crate::commands::{queue, ready, show_toast, submit, with_project};
use crate::{WorkStore, modals};

/// The branch `git init` falls back to when the user's git configuration names none.
const FALLBACK_BRANCH: &str = "main";

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
            let Some(fs) = WorkStore::global(cx).map(|store| store.read(cx).fs()) else {
                return;
            };
            let Some(write) = queue(workspace, cx, move |tree, ctx| {
                model::project_new(
                    tree,
                    ctx,
                    NewProject {
                        title: title.clone(),
                        ..NewProject::default()
                    },
                )
            }) else {
                return;
            };
            cx.spawn(async move |workspace, cx| {
                let failure = match write.await {
                    Err(error) => Some(error.to_string()),
                    Ok(change) => fs
                        .git_init(&change.dir, FALLBACK_BRANCH.to_string())
                        .await
                        .err()
                        .map(|error| {
                            format!("The project was created, but git init failed: {error}")
                        }),
                };
                if let Some(message) = failure {
                    workspace
                        .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                        .ok();
                }
            })
            .detach();
        }),
        window,
        cx,
    );
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
