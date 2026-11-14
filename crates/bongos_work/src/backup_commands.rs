//! The handlers of `project: backup status`, which shows the status the repository computes in
//! a read-only buffer, and `project: backup now`, which takes a snapshot at once and pushes it.

use bongos_project_repo::{ProjectRepo, ProjectRepositorySettings, SnapshotOutcome};
use editor::{Editor, MultiBuffer};
use gpui::{AppContext as _, Context, Window};
use language::Capability;
use settings::Settings as _;
use util::ResultExt as _;
use workspace::Workspace;

use crate::actions::project;
use crate::commands::show_toast;
use crate::working_copy_commands::{now, with_repo};

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &project::BackupStatus, window, cx| backup_status(w, window, cx))
        .register_action(|w, _: &project::BackupNow, window, cx| backup_now(w, window, cx));
}

/// The repository with the backup settings of this moment.
fn configured(repo: ProjectRepo, cx: &mut Context<Workspace>) -> ProjectRepo {
    repo.set_backup_options(ProjectRepositorySettings::get_global(cx).backup_options());
    repo
}

fn backup_status(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_repo(workspace, window, cx, |repo, _, window, cx| {
        let repo = configured(repo, cx);
        let status = cx.background_spawn(async move { repo.backup_status(now()).await });
        cx.spawn_in(window, async move |workspace, cx| match status.await {
            Ok(status) => {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        open_read_only(workspace, status.render(), window, cx)
                    })
                    .log_err();
            }
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, format!("Backup status: {error:#}"), cx)
                    })
                    .log_err();
            }
        })
        .detach();
    });
}

/// Opens `text` in a new read-only buffer titled "Backup status".
fn open_read_only(
    workspace: &mut Workspace,
    text: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(
                        workspace,
                        "Could not open the backup status".to_string(),
                        cx,
                    )
                })
                .log_err();
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                let multi_buffer = cx.new(|cx| {
                    MultiBuffer::singleton(buffer, cx).with_title("Backup status".to_string())
                });
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
            .log_err();
    })
    .detach();
}

/// What a person is told after `project: backup now`.
pub(crate) fn backup_now_message(outcome: &SnapshotOutcome, has_remote: bool) -> String {
    let pushing = if has_remote {
        "; pushing it to the remote"
    } else {
        "; not backed up: no remote"
    };
    match outcome {
        SnapshotOutcome::Taken { commit } => {
            format!(
                "Snapshot {} taken{pushing}",
                commit.get(..10).unwrap_or(commit)
            )
        }
        SnapshotOutcome::Unchanged { .. } => {
            format!("Nothing changed since the last snapshot{pushing}")
        }
        SnapshotOutcome::NothingChanged => "Nothing changed since the last snapshot".to_string(),
        SnapshotOutcome::TooSoon { seconds_left } => {
            format!("The last snapshot is too recent; try again in {seconds_left} seconds")
        }
        SnapshotOutcome::Stopped => "Backups are stopped: bongos is shutting down".to_string(),
    }
}

fn backup_now(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_repo(workspace, window, cx, |repo, _, window, cx| {
        let repo = configured(repo, cx);
        let has_remote = repo.location().remote.is_some();
        let taken = cx.background_spawn(async move { repo.backup_now(now()).await });
        cx.spawn_in(window, async move |workspace, cx| {
            let message = match taken.await {
                Ok(outcome) => backup_now_message(&outcome, has_remote),
                Err(error) => format!("Backup now: {error:#}"),
            };
            workspace
                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                .log_err();
        })
        .detach();
    });
}
