//! The handlers of `project: edit`, `project: open working copy`, `project: abandon`,
//! `project: open repository`, `project: land`, `project: check` and `project: approve`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ::project::File as ProjectFile;
use bongos_project_repo::{
    ChangeFilter, ChangeRow, CheckOutcome, LandMode, LandOptions, LandRequest, LandingResult,
    ProjectRepo, ProjectRepositorySettings, clean_name, fresh_name,
};
use bongos_project_repo_model::ChangeMeta;
use bongos_project_repo_model::checks::SizeLimits;
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use chrono::{DateTime, Utc};
use editor::Editor;
use gpui::{AppContext as _, Context, Window};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::{MultiWorkspace, OpenMode, OpenOptions, Workspace};

use crate::actions::project;
use crate::commands::{keyboard_author, keyboard_context, show_toast};
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
        })
        .register_action(|w, _: &project::Land, window, cx| land(w, LandMode::Land, window, cx))
        .register_action(|w, _: &project::Check, window, cx| {
            land(w, LandMode::CheckOnly, window, cx)
        })
        .register_action(|w, _: &project::Approve, window, cx| approve(w, window, cx));
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

/// What a landing reads from the `project_repository` settings.
pub(crate) fn land_options(settings: &ProjectRepositorySettings) -> LandOptions {
    LandOptions {
        squash: settings.squash,
        queue_limit: Duration::from_secs(u64::from(settings.queue_limit_seconds)),
        keep_landed: settings.keep_landed,
        branch_prefix: settings.branch_prefix.clone(),
        size_limits: SizeLimits {
            text_file_limit_bytes: settings.text_file_limit_bytes,
            image_limit_bytes: settings.image_limit_bytes,
        },
        secret_patterns: settings.secret_patterns.clone(),
    }
}

/// The change among `rows` whose working copy holds `path`.
pub(crate) fn change_at(repo: &ProjectRepo, rows: &[ChangeRow], path: &Path) -> Option<ChangeRow> {
    rows.iter()
        .find(|row| path.starts_with(repo.working_copy_path(&row.name)))
        .cloned()
}

/// What a person is told when a landing or a check of the working copy `name` ends.
pub(crate) fn landing_message(name: &str, result: &LandingResult) -> String {
    match result {
        LandingResult::Landed { commit } => {
            format!("Landed {name} as {}", &commit[..commit.len().min(8)])
        }
        LandingResult::Abandoned => format!("{name} was abandoned"),
        LandingResult::NotRequested => format!("{name} was not requested for landing"),
        LandingResult::Checked { checks, .. } => {
            let lines: Vec<String> = checks
                .iter()
                .map(|result| match &result.outcome {
                    CheckOutcome::Pass => format!("{}: pass", result.check),
                    CheckOutcome::Fail { message, .. } => {
                        format!("{}: fail, {message}", result.check)
                    }
                    CheckOutcome::NoVerdict { reason, .. } => {
                        format!("{}: no verdict, {reason}", result.check)
                    }
                })
                .collect();
            format!("Checks of {name}: {}", lines.join("; "))
        }
        LandingResult::Refused { reasons } => {
            format!("{name} was not landed: {}", reasons.join("; "))
        }
        LandingResult::Conflict { files } => format!(
            "{name} conflicts with master in {}; resolve the markers and land again",
            files.join(", ")
        ),
        LandingResult::NoVerdict { reason } => format!("{name} is waiting: {reason}"),
    }
}

/// The file of the buffer of the active editor.
fn active_path(workspace: &Workspace, cx: &gpui::App) -> Option<PathBuf> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let buffer = buffer.read(cx);
    let file = ProjectFile::from_dyn(buffer.file())?;
    Some(file.worktree.read(cx).absolutize(&file.path))
}

/// `project: land` and `project: check`: the working copy of the current buffer, else one picked.
fn land(
    workspace: &mut Workspace,
    mode: LandMode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(path) = active_path(workspace, cx) {
        for loaded in open_repos(cx) {
            let repo = loaded.repo().clone();
            let rows = repo
                .changes(&ChangeFilter::default())
                .log_err()
                .unwrap_or_default();
            if let Some(row) = change_at(&repo, &rows, &path) {
                ask_and_land(repo, row, mode, workspace, window, cx);
                return;
            }
        }
    }
    let placeholder = match mode {
        LandMode::Land => "Land which working copy?",
        LandMode::CheckOnly => "Check which working copy?",
    };
    pick_working_copy(
        workspace,
        window,
        cx,
        placeholder,
        move |repo, row, workspace, window, cx| {
            ask_and_land(repo, row, mode, workspace, window, cx)
        },
    );
}

/// Asks for the subject of a landing (a check needs none), then lands or checks `row`.
fn ask_and_land(
    repo: ProjectRepo,
    row: ChangeRow,
    mode: LandMode,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match mode {
        LandMode::CheckOnly => start_landing(repo, row, mode, String::new(), window, cx),
        LandMode::Land => {
            let initial = format!("Change {}", row.name);
            modals::ask(
                workspace,
                "Subject of the change",
                &initial,
                false,
                Box::new(move |subject, _, window, cx| {
                    start_landing(repo, row, mode, subject, window, cx)
                }),
                window,
                cx,
            );
        }
    }
}

fn start_landing(
    repo: ProjectRepo,
    row: ChangeRow,
    mode: LandMode,
    subject: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    repo.set_land_options(land_options(ProjectRepositorySettings::get_global(cx)));
    let request = LandRequest {
        meta: ChangeMeta::new(subject, keyboard_author(window, cx)),
        mode,
    };
    let copy = repo.working_copy_path(&row.name);
    // A person's `project: land` of the person's own change approves it.
    let landing = match mode {
        LandMode::Land => {
            repo.land_by_person(&row.change_id, request, keyboard_context(window, cx), cx)
        }
        LandMode::CheckOnly => repo.land(&row.change_id, request, cx),
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let message = match landing.await {
            Ok(result) => {
                if let LandingResult::Conflict { files } = &result {
                    for file in files {
                        let path = copy.join(file);
                        let opening = workspace.update_in(cx, |workspace, window, cx| {
                            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
                        });
                        if let Ok(opening) = opening {
                            opening.await.log_err();
                        }
                    }
                }
                landing_message(&row.name, &result)
            }
            Err(error) => format!("Landing {} failed: {error:#}", row.name),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .log_err();
    })
    .detach();
}

/// `project: approve`: the change of the current buffer, else the outside commit on master once
/// its checks passed, else a change picked. A person in a change's buffer approves that change,
/// never an outside commit they did not look at. The approval is the person's at this window; a
/// landing waiting for it goes on.
fn approve(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(path) = active_path(workspace, cx) {
        for loaded in open_repos(cx) {
            let repo = loaded.repo().clone();
            let rows = repo
                .changes(&ChangeFilter::default())
                .log_err()
                .unwrap_or_default();
            if let Some(row) = change_at(&repo, &rows, &path) {
                approve_change(repo, row, window, cx);
                return;
            }
        }
    }
    for loaded in open_repos(cx) {
        let repo = loaded.repo().clone();
        if let Ok(Some(outside)) = repo.outside_change()
            && outside.passed
        {
            let Some(marker) = keyboard_context(window, cx).take_attestation() else {
                return;
            };
            cx.spawn_in(window, async move |workspace, cx| {
                let message = match repo.approve_outside(marker).await {
                    Ok(commit) => {
                        let message = format!(
                            "Approved the outside commit {} on master",
                            short_commit(&commit)
                        );
                        log::info!("{message}");
                        message
                    }
                    Err(error) => format!("{error:#}"),
                };
                workspace
                    .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                    .log_err();
            })
            .detach();
            return;
        }
    }
    pick_working_copy(
        workspace,
        window,
        cx,
        "Approve which change?",
        |repo, row, _, window, cx| approve_change(repo, row, window, cx),
    );
}

/// The first eight hex digits of `commit`.
fn short_commit(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

fn approve_change(
    repo: ProjectRepo,
    row: ChangeRow,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let ctx = keyboard_context(window, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let message = match repo.approve(&row.change_id, ctx, now()).await {
            Ok(approval) => format!(
                "Approved change {} on {} at {} ({})",
                row.change_id,
                row.branch,
                short_commit(&approval.commit),
                approval.name
            ),
            Err(error) => format!("Approving {} failed: {error:#}", row.name),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .log_err();
    })
    .detach();
}
