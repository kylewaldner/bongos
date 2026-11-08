//! The artifact commands (D22): `artifact: hide`, `share`, `hide all`, `share all` and `link`,
//! `task: set default visibility`, `task: think`, and in debug builds `work: dump session
//! context`. Hiding and sharing write `artifacts.toml` only; what a chat session is told comes from
//! `bongos_work_model::session_context`, which leaves every human-only artifact out.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bongos_work_model::Visibility;
use bongos_work_model::artifacts::task_artifacts;
use bongos_work_model::commands::{self as model, visibility_name};
use bongos_work_model::store::{Task, WorkTree};
use fs::{CreateOptions, Fs};
use gpui::{Context, Window};
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::actions::{artifact, task};
use crate::commands::{ready, show_toast, submit, with_task};
use crate::{Focus, modals};

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &artifact::Hide, window, cx| {
            hide_or_share(w, Visibility::HumanOnly, window, cx)
        })
        .register_action(|w, _: &artifact::Share, window, cx| {
            hide_or_share(w, Visibility::Shared, window, cx)
        })
        .register_action(|w, _: &artifact::HideAll, window, cx| {
            set_all(w, Visibility::HumanOnly, window, cx)
        })
        .register_action(|w, _: &artifact::ShareAll, window, cx| {
            set_all(w, Visibility::Shared, window, cx)
        })
        .register_action(|w, _: &artifact::Link, window, cx| link(w, window, cx))
        .register_action(|w, _: &task::SetDefaultVisibility, window, cx| {
            set_default_visibility(w, window, cx)
        })
        .register_action(|w, _: &task::Think, window, cx| think(w, window, cx));
    #[cfg(debug_assertions)]
    workspace.register_action(|w, _: &crate::work::DumpSessionContext, _, cx| {
        dump_session_context(w, cx)
    });
}

/// The manifest keys of the artifacts of `task` that do not have `visibility` yet: those
/// `artifact: hide` (for human-only) or `artifact: share` (for shared) can change.
fn changeable(task: &Task, visibility: Visibility) -> Vec<(String, String)> {
    task_artifacts(task)
        .artifacts
        .into_iter()
        .filter(|artifact| artifact.visibility != Some(visibility))
        .map(|artifact| {
            let label = if artifact.description.is_empty() {
                artifact.path.clone()
            } else {
                format!("{}: {}", artifact.path, artifact.description)
            };
            (artifact.path, label)
        })
        .collect()
}

fn has_changeable(task: &Task, visibility: Visibility) -> bool {
    task.file.is_some() && !changeable(task, visibility).is_empty()
}

fn hide_or_share(
    workspace: &mut Workspace,
    visibility: Visibility,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (task_placeholder, none, artifact_placeholder) = match visibility {
        Visibility::HumanOnly => (
            "Hide an artifact of which task?",
            "No task has a shared artifact",
            "Hide which artifact from chat sessions?",
        ),
        Visibility::Shared => (
            "Share an artifact of which task?",
            "No task has a human-only artifact",
            "Share which artifact with chat sessions?",
        ),
    };
    with_task(
        workspace,
        window,
        cx,
        task_placeholder,
        none,
        move |task, _| has_changeable(task, visibility),
        Box::new(move |id, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let Some(task) = tree.task_by_id(&id) else {
                return;
            };
            let (paths, labels): (Vec<String>, Vec<String>) =
                changeable(task, visibility).into_iter().unzip();
            modals::pick(
                workspace,
                artifact_placeholder,
                labels,
                0,
                Box::new(move |index, workspace, _, cx| {
                    let path = paths[index].clone();
                    submit(workspace, cx, move |tree, ctx| match visibility {
                        Visibility::HumanOnly => model::artifact_hide(tree, ctx, &id, &path),
                        Visibility::Shared => model::artifact_share(tree, ctx, &id, &path),
                    })
                }),
                window,
                cx,
            )
        }),
    );
}

fn set_all(
    workspace: &mut Workspace,
    visibility: Visibility,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (placeholder, none) = match visibility {
        Visibility::HumanOnly => (
            "Hide every artifact of which task?",
            "No task has a shared artifact",
        ),
        Visibility::Shared => (
            "Share every artifact of which task?",
            "No task has a human-only artifact",
        ),
    };
    with_task(
        workspace,
        window,
        cx,
        placeholder,
        none,
        move |task, _| has_changeable(task, visibility),
        Box::new(move |id, workspace, _, cx| {
            submit(workspace, cx, move |tree, ctx| match visibility {
                Visibility::HumanOnly => model::artifact_hide_all(tree, ctx, &id),
                Visibility::Shared => model::artifact_share_all(tree, ctx, &id),
            })
        }),
    );
}

fn link(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Add a link to which task?",
        "There is no task",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            modals::ask(
                workspace,
                "URL of the document",
                "",
                false,
                Box::new(move |url, workspace, window, cx| {
                    modals::ask(
                        workspace,
                        "Title of the link; empty uses the URL",
                        "",
                        true,
                        Box::new(move |title, workspace, _, cx| {
                            submit(workspace, cx, move |tree, ctx| {
                                model::artifact_link(tree, ctx, &id, &url, &title)
                            })
                        }),
                        window,
                        cx,
                    )
                }),
                window,
                cx,
            )
        }),
    );
}

const VISIBILITIES: [Visibility; 2] = [Visibility::Shared, Visibility::HumanOnly];

fn set_default_visibility(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_task(
        workspace,
        window,
        cx,
        "Set the default visibility of which task?",
        "There is no task",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let Some(current) = tree
                .task_by_id(&id)
                .and_then(|task| task.file.as_ref())
                .map(|file| file.default_visibility)
            else {
                return;
            };
            let labels = VISIBILITIES
                .iter()
                .map(|visibility| {
                    let name = visibility_name(*visibility);
                    if *visibility == current {
                        format!("{name} (current)")
                    } else {
                        name.to_string()
                    }
                })
                .collect();
            modals::pick(
                workspace,
                "Visibility of files new to the task",
                labels,
                VISIBILITIES
                    .iter()
                    .position(|visibility| *visibility != current)
                    .unwrap_or(0),
                Box::new(move |choice, workspace, _, cx| {
                    let visibility = VISIBILITIES[choice];
                    submit(workspace, cx, move |tree, ctx| {
                        model::set_default_visibility(tree, ctx, &id, visibility)
                    })
                }),
                window,
                cx,
            )
        }),
    );
}

/// The focused task's id, when it names a task in `tree`.
fn focused(tree: &WorkTree, cx: &gpui::App) -> Option<String> {
    let focus = Focus::global(cx)?;
    let id = focus.read(cx).task_id()?.to_string();
    tree.task_by_id(&id).is_some().then_some(id)
}

/// `task: think`: a new, empty `untitled-<n>.md` in the task's directory, opened in an editor.
/// Nothing else starts: no chat, agent or process.
fn think(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    if let Some(id) = focused(&tree, cx) {
        open_untitled(workspace, &tree, &id, window, cx);
        return;
    }
    with_task(
        workspace,
        window,
        cx,
        "Think about which task?",
        "There is no task",
        |task, _| task.file.is_some(),
        Box::new(|id, workspace, window, cx| {
            if let Some(tree) = ready(workspace, cx) {
                open_untitled(workspace, &tree, &id, window, cx);
            }
        }),
    );
}

fn open_untitled(
    workspace: &mut Workspace,
    tree: &WorkTree,
    id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(dir) = tree.task_by_id(id).map(|task| task.dir.clone()) else {
        return;
    };
    let fs = workspace.app_state().fs.clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let opened = async {
            let path = new_untitled(fs.as_ref(), &dir).await?;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        path,
                        OpenOptions {
                            visible: Some(OpenVisible::None),
                            ..OpenOptions::default()
                        },
                        window,
                        cx,
                    )
                })?
                .await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = opened {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(
                        workspace,
                        format!("Could not open a new document: {error}"),
                        cx,
                    )
                })
                .ok();
        }
    })
    .detach();
}

/// Creates the first free `untitled-<n>.md` in `dir`, empty, and returns its path.
pub(crate) async fn new_untitled(fs: &dyn Fs, dir: &Path) -> Result<PathBuf> {
    fs.create_dir(dir).await?;
    let mut number = 1;
    loop {
        let path = dir.join(format!("untitled-{number}.md"));
        if fs.metadata(&path).await?.is_none() {
            fs.create_file(
                &path,
                CreateOptions {
                    overwrite: false,
                    ignore_if_exists: false,
                },
            )
            .await?;
            return Ok(path);
        }
        number += 1;
    }
}

/// The file `work: dump session context` writes in the logs directory.
#[cfg(debug_assertions)]
pub(crate) const DUMP_FILE: &str = "work-session-context.json";

/// Writes the session context of the task in `task_dir` as JSON to [`DUMP_FILE`] in `logs_dir`,
/// and returns its path.
#[cfg(debug_assertions)]
pub(crate) fn write_session_context(task_dir: &Path, logs_dir: &Path) -> Result<PathBuf> {
    let context = bongos_work_model::session_context(task_dir)?;
    let json = serde_json::to_string_pretty(&context)?;
    std::fs::create_dir_all(logs_dir)?;
    let path = logs_dir.join(DUMP_FILE);
    std::fs::write(&path, json)?;
    Ok(path)
}

#[cfg(debug_assertions)]
fn dump_session_context(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let Some(dir) = focused(&tree, cx).and_then(|id| Some(tree.task_by_id(&id)?.dir.clone()))
    else {
        show_toast(workspace, "No task is focused".to_string(), cx);
        return;
    };
    let message = match write_session_context(&dir, paths::logs_dir()) {
        Ok(path) => format!("Wrote {}", path.display()),
        Err(error) => format!("Could not write the session context: {error}"),
    };
    show_toast(workspace, message, cx);
}
