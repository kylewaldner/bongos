//! The handlers of `project: edit`, `project: open working copy`, `project: abandon` and
//! `project: open repository`.

use std::path::PathBuf;

use bongos_project_repo::{
    ChangeFilter, ChangeRow, ProjectRepo, ProjectRepositorySettings, clean_name, fresh_name,
};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use chrono::{DateTime, Utc};
use gpui::{AppContext as _, Context, Window};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenMode, Workspace};

use crate::actions::project;
use crate::commands::{keyboard_author, show_toast};
use crate::modals;
use crate::project_repos::open_repos;

/// The time the app acts at; every function that decides something takes it as a parameter.
pub(crate) fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &project::Edit, window, cx| edit(w, window, cx))
        .register_action(|w, _: &project::OpenWorkingCopy, window, cx| {
            open_working_copy(w, window, cx)
        })
        .register_action(|w, _: &project::Abandon, window, cx| abandon(w, window, cx))
        .register_action(|w, _: &project::OpenRepository, window, cx| {
            open_repository(w, window, cx)
        });
}

/// A row of a list of working copies: its name, who made it, how long it has been idle, and
/// "stale" when it has been idle for `stale_after_days` days or more at `now`.
pub(crate) fn working_copy_label(
    row: &ChangeRow,
    now: DateTime<Utc>,
    stale_after_days: u32,
) -> String {
    let idle = now.signed_duration_since(row.updated).num_days().max(0);
    let mut label = format!("{} ({}, idle {idle}d", row.name, row.author.name);
    if row.is_stale(now, stale_after_days) {
        label.push_str(", stale");
    }
    label.push(')');
    label
}

/// Runs `then` with the project repository to act on: the only open one, or the one picked.
fn with_repo(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(ProjectRepo, &mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) {
    let repos = open_repos(cx);
    match repos.len() {
        0 => show_toast(
            workspace,
            "No project repository is open (project_repository.enabled)".to_string(),
            cx,
        ),
        1 => then(repos[0].repo().clone(), workspace, window, cx),
        _ => {
            let labels = repos
                .iter()
                .map(|loaded| {
                    loaded
                        .repo()
                        .location()
                        .folder
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| loaded.repo().location().id.clone())
                })
                .collect();
            modals::pick(
                workspace,
                "Which project?",
                labels,
                0,
                Box::new(move |index, workspace, window, cx| {
                    if let Some(loaded) = repos.get(index) {
                        then(loaded.repo().clone(), workspace, window, cx);
                    }
                }),
                window,
                cx,
            );
        }
    }
}

/// Opens `path` as a workspace of this window and activates it.
fn open_in_new_workspace(path: PathBuf, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
        return;
    };
    cx.spawn_in(window, async move |_, cx| {
        let opening = handle.update(cx, |multi, window, cx| {
            multi.open_project(vec![path], OpenMode::Activate, window, cx)
        });
        if let Ok(opening) = opening {
            opening.await.log_err();
        }
    })
    .detach();
}

fn edit(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_repo(workspace, window, cx, |repo, workspace, window, cx| {
        modals::ask(
            workspace,
            "Name of the working copy",
            &fresh_name(),
            false,
            Box::new(move |name, _, window, cx| {
                let prefix = ProjectRepositorySettings::get_global(cx)
                    .branch_prefix
                    .clone();
                let author = keyboard_author(window, cx);
                let created = cx.background_spawn(async move {
                    let name = clean_name(&name)?;
                    repo.create_working_copy(&name, &prefix, &author, now())
                        .await
                });
                cx.spawn_in(window, async move |workspace, cx| match created.await {
                    Ok(copy) => {
                        workspace
                            .update_in(cx, |_, window, cx| {
                                open_in_new_workspace(copy.path.join(DEFINITIONS_DIR), window, cx)
                            })
                            .log_err();
                    }
                    Err(error) => {
                        workspace
                            .update(cx, |workspace, cx| {
                                show_toast(workspace, format!("{error:#}"), cx)
                            })
                            .log_err();
                    }
                })
                .detach();
            }),
            window,
            cx,
        );
    });
}

/// Picks one of the open working copies of the project and runs `then` on it.
fn pick_working_copy(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    placeholder: &'static str,
    then: impl FnOnce(ProjectRepo, ChangeRow, &mut Workspace, &mut Window, &mut Context<Workspace>)
    + 'static,
) {
    with_repo(workspace, window, cx, move |repo, workspace, window, cx| {
        let rows = repo
            .changes(&ChangeFilter::default())
            .log_err()
            .unwrap_or_default();
        if rows.is_empty() {
            show_toast(workspace, "There is no working copy".to_string(), cx);
            return;
        }
        let stale_days = ProjectRepositorySettings::get_global(cx).stale_after_days;
        let labels = rows
            .iter()
            .map(|row| working_copy_label(row, now(), stale_days))
            .collect();
        modals::pick(
            workspace,
            placeholder,
            labels,
            0,
            Box::new(move |index, workspace, window, cx| {
                if let Some(row) = rows.into_iter().nth(index) {
                    then(repo, row, workspace, window, cx);
                }
            }),
            window,
            cx,
        );
    });
}

fn open_working_copy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    pick_working_copy(
        workspace,
        window,
        cx,
        "Open which working copy?",
        |repo, row, _, window, cx| {
            open_in_new_workspace(
                repo.working_copy_path(&row.name).join(DEFINITIONS_DIR),
                window,
                cx,
            );
        },
    );
}

fn abandon(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    pick_working_copy(
        workspace,
        window,
        cx,
        "Abandon which working copy?",
        |repo, row, workspace, window, cx| {
            let name = row.name.clone();
            modals::pick(
                workspace,
                format!("Abandon {name}? Its branch and files are deleted; master is untouched"),
                vec![format!("Abandon {name}"), "Keep it".to_string()],
                1,
                Box::new(move |choice, _, _, cx| {
                    if choice != 0 {
                        return;
                    }
                    let abandoned = repo.abandon(&row.change_id, cx);
                    cx.spawn(async move |workspace, cx| {
                        let message = match abandoned.await {
                            Ok(()) => format!("Abandoned {name}"),
                            Err(error) => error.to_string(),
                        };
                        workspace
                            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                            .log_err();
                    })
                    .detach();
                }),
                window,
                cx,
            );
        },
    );
}

fn open_repository(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_repo(workspace, window, cx, |repo, _, window, cx| {
        open_in_new_workspace(repo.location().repository.join(DEFINITIONS_DIR), window, cx);
    });
}
