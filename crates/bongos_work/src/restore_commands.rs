//! The handlers of `project: add existing`, which adds a project repository from a folder or a
//! remote address, and `project: restore` (and `project: restore from`, which picks the source),
//! which shows what a restore replaces in a read-only buffer and asks before it runs.

use std::path::{Path, PathBuf};

use bongos_project_repo::{
    AddExistingRequest, AddExistingSource, AddedProject, Git, ProjectRepo,
    ProjectRepositorySettings, RepositoryLocations, RestoreOutcome, RestorePreview, RestoreSource,
    registered_checks,
};
use bongos_work_model::slug::slugify;
use gpui::{AppContext as _, Context, Window};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::Workspace;

use crate::actions::project;
use crate::backup_commands::{configured, open_read_only};
use crate::commands::{ready, show_toast};
use crate::modals;
use crate::working_copy_commands::{now, with_repo};

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &project::AddExisting, window, cx| add_existing(w, window, cx))
        .register_action(|w, _: &project::Restore, window, cx| restore(w, false, window, cx))
        .register_action(|w, _: &project::RestoreFrom, window, cx| restore(w, true, window, cx));
}

/// Where the text typed for `project: add existing` points: an existing folder holding a
/// project or a repository, or else a remote address to clone into a new folder under the
/// projects folder, named after the address.
pub(crate) fn add_source(
    text: &str,
    repository_template: &str,
    project_folder: impl Fn(&str) -> PathBuf,
) -> AddExistingSource {
    let expanded = match text.strip_prefix("~/") {
        Some(rest) => paths::home_dir().join(rest),
        None => PathBuf::from(text),
    };
    let is_project = |path: &Path| {
        path.join(".git").exists()
            || PathBuf::from(bongos_project_repo_model::layout::resolve_template(
                repository_template,
                &path.to_string_lossy(),
            ))
            .join(".git")
            .exists()
    };
    if expanded.is_absolute() && expanded.is_dir() && is_project(&expanded) {
        return AddExistingSource::Path { path: expanded };
    }
    let name = text
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default();
    let name = name.strip_suffix(".git").unwrap_or(name);
    let slug = slugify(name);
    let slug = if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    };
    AddExistingSource::Remote {
        address: text.to_string(),
        folder: project_folder(&slug),
    }
}

/// What a person is told after `project: add existing`.
pub(crate) fn added_message(added: &AddedProject) -> String {
    let folder = added.repo.location().folder.display();
    let mut message = format!("Added the project at {folder}");
    if !added.problems.is_empty() {
        message.push_str(&format!(
            "; master fails the checks ({}), so it is not verified yet",
            added.problems.join("; ")
        ));
    }
    if added.has_snapshot {
        message.push_str(
            "; a snapshot of its state is there: once it is open, project: restore (C-c w g R) rebuilds it",
        );
    }
    for note in &added.notes {
        message.push_str(&format!("; {note}"));
    }
    message
}

fn add_existing(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if ready(workspace, cx).is_none() {
        return;
    }
    if !ProjectRepositorySettings::get_global(cx).enabled {
        show_toast(
            workspace,
            "Project repositories are off (project_repository.enabled)".to_string(),
            cx,
        );
        return;
    }
    modals::ask(
        workspace,
        "Project folder, repository folder or remote address",
        "",
        false,
        Box::new(|text, _, _, cx| {
            let settings = ProjectRepositorySettings::get_global(cx).clone();
            let request = AddExistingRequest {
                source: add_source(&text, &settings.layout.repository, |slug| {
                    settings.project_folder(slug)
                }),
                layout: settings.layout.clone(),
                state_branch: settings.state_branch.clone(),
                remote: None,
                checks: Some(registered_checks(cx)),
                timeout: None,
                secret_patterns: settings.secret_patterns.clone(),
            };
            let added = cx.background_spawn(async move {
                let mut locations = RepositoryLocations::load(RepositoryLocations::default_path())?;
                let added = ProjectRepo::add_existing(request, &mut locations, Git::locate).await?;
                // The project opens like any other; this process lets go of its state now.
                anyhow::Ok((added_message(&added), added.repo.location().clone()))
            });
            cx.spawn(async move |workspace, cx| {
                let message = match added.await {
                    Ok((message, location)) => {
                        match crate::oneoffs::open_location(location, cx).await {
                            Ok(()) => message,
                            Err(error) => format!("{message}; opening it failed: {error:#}"),
                        }
                    }
                    Err(error) => format!("Add existing: {error:#}"),
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
}

/// What a person is told after a restore.
pub(crate) fn restore_message(outcome: &RestoreOutcome) -> String {
    match outcome {
        RestoreOutcome::Restored { preview, replaced } => format!(
            "Restored the state from the {}; the replaced state is kept as state/{}/{replaced}",
            preview_label(preview),
            bongos_project_repo::REPLACED_DIR,
        ),
        RestoreOutcome::Refused { activity } => format!(
            "Not restored: work is running ({}); stop it at a job boundary first",
            activity.join("; ")
        ),
    }
}

fn preview_label(preview: &RestorePreview) -> String {
    bongos_project_repo::RestoreCandidate {
        kind: preview.kind,
        source: preview.source.clone(),
        taken_at: preview.taken_at,
    }
    .label()
}

fn restore(
    workspace: &mut Workspace,
    pick: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_repo(workspace, window, cx, move |repo, workspace, window, cx| {
        let repo = configured(repo, cx);
        if !pick {
            preview_and_ask(repo, RestoreSource::Newest, workspace, window, cx);
            return;
        }
        let listing = repo.clone();
        let candidates = cx.background_spawn(async move { listing.restore_candidates().await });
        cx.spawn_in(window, async move |workspace, cx| {
            let candidates = match candidates.await {
                Ok(candidates) => candidates,
                Err(error) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_toast(workspace, format!("Restore: {error:#}"), cx)
                        })
                        .log_err();
                    return;
                }
            };
            workspace
                .update_in(cx, |workspace, window, cx| {
                    if candidates.candidates.is_empty() {
                        let mut message =
                            "There is no snapshot or replaced state to restore".to_string();
                        for note in &candidates.notes {
                            message.push_str(&format!("; {note}"));
                        }
                        show_toast(workspace, message, cx);
                        return;
                    }
                    let labels = candidates
                        .candidates
                        .iter()
                        .map(|candidate| candidate.label())
                        .collect();
                    let sources: Vec<RestoreSource> = candidates
                        .candidates
                        .iter()
                        .map(|candidate| candidate.source.clone())
                        .collect();
                    modals::pick(
                        workspace,
                        "Restore from",
                        labels,
                        0,
                        Box::new(move |index, workspace, window, cx| {
                            if let Some(source) = sources.get(index) {
                                preview_and_ask(repo, source.clone(), workspace, window, cx);
                            }
                        }),
                        window,
                        cx,
                    );
                })
                .log_err();
        })
        .detach();
    });
}

/// Shows what restoring from `source` replaces, then asks: to restore, or while work runs, to
/// stop it at a job boundary.
fn preview_and_ask(
    repo: ProjectRepo,
    source: RestoreSource,
    _workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let previewing = repo.clone();
    let preview = cx.background_spawn(async move { previewing.restore_preview(&source).await });
    cx.spawn_in(window, async move |workspace, cx| {
        let preview = match preview.await {
            Ok(preview) => preview,
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, format!("Restore: {error:#}"), cx)
                    })
                    .log_err();
                return;
            }
        };
        workspace
            .update_in(cx, |workspace, window, cx| {
                open_read_only(workspace, "Restore", preview.render(), window, cx);
                ask(repo, preview, workspace, window, cx);
            })
            .log_err();
    })
    .detach();
}

fn ask(
    repo: ProjectRepo,
    preview: RestorePreview,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !preview.activity.is_empty() {
        modals::pick(
            workspace,
            "Work is running in this project; restore refuses until it stops",
            vec![
                "Stop it at a job boundary".to_string(),
                "Leave it running".to_string(),
            ],
            1,
            Box::new(move |choice, workspace, _, cx| {
                if choice != 0 {
                    return;
                }
                let problems = repo.stop_at_job_boundary();
                let message = if problems.is_empty() {
                    "Asked the running work to stop at its next job boundary; restore again once it stopped".to_string()
                } else {
                    format!("Could not ask all work to stop: {}", problems.join("; "))
                };
                show_toast(workspace, message, cx);
            }),
            window,
            cx,
        );
        return;
    }
    let label = preview_label(&preview);
    let source = preview.source;
    modals::pick(
        workspace,
        format!("Replace the live state with the {label}?"),
        vec!["Restore".to_string(), "Cancel".to_string()],
        1,
        Box::new(move |choice, _, _, cx| {
            if choice != 0 {
                return;
            }
            let restored = cx.background_spawn(async move { repo.restore(&source, now()).await });
            cx.spawn(async move |workspace, cx| {
                let message = match restored.await {
                    Ok(outcome) => restore_message(&outcome),
                    Err(error) => format!("Restore: {error:#}"),
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
}
