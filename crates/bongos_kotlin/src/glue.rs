//! Connects `KotlinProjectManager` to real projects: one manager per local project, fed by the
//! project's `LspStore` and worktrees, restarting through the store.

use std::rc::Rc;

use gpui::{App, AppContext as _, Context, Entity, WeakEntity};
use lsp::{LanguageServerId, LanguageServerName};
use project::{Project, worktree_store::WorktreeStoreEvent};
use settings::WorktreeId;
use workspace::{
    Toast, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::{
    KotlinDirs, KotlinUpdates, KotlinUpdatesEvent, ManifestNoticePending,
    manager::{ADAPTER_NAME, KotlinEvent, KotlinHost, KotlinProjectManager, KotlinRegistry},
};

const KOTLIN_LANGUAGE: &str = "Kotlin";

struct ProjectHost {
    project: WeakEntity<Project>,
}

impl KotlinHost for ProjectHost {
    fn restart(&self, worktree_id: WorktreeId, _server_id: Option<LanguageServerId>, cx: &mut App) {
        // Deferred: a restart the policy asks for runs inside a project's `LspStore` event, while
        // the project itself is being updated.
        let project = self.project.clone();
        cx.defer(move |cx| restart_worktree(&project, worktree_id, cx));
    }
}

/// Stops the worktree's Kotlin server and starts it again for its open Kotlin buffers.
fn restart_worktree(project: &WeakEntity<Project>, worktree_id: WorktreeId, cx: &mut App) {
    let Some(project) = project.upgrade() else {
        return;
    };
    let buffers: Vec<_> = project
        .read(cx)
        .buffer_store()
        .read(cx)
        .buffers()
        .filter(|buffer| {
            let buffer = buffer.read(cx);
            buffer
                .language()
                .is_some_and(|language| language.name().as_ref() == KOTLIN_LANGUAGE)
                && buffer
                    .file()
                    .is_some_and(|file| file.worktree_id(cx) == worktree_id)
        })
        .collect();
    // By worktree, not by `server_id`: a server whose start failed, or that has not been
    // added yet, has no id in the manager but still has to be replaced. With no Kotlin buffer
    // open the server just stops: the next buffer starts the effective one.
    project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
        lsp_store.restart_language_server_for_worktree(
            worktree_id,
            LanguageServerName::new_static(ADAPTER_NAME),
            buffers,
            cx,
        );
    });
}

/// Gives every local project a manager. Call before any project is created.
pub fn attach_managers(dirs: KotlinDirs, cx: &mut App) {
    cx.observe_new::<Project>(move |project, _window, cx| {
        if !project.is_local() {
            return;
        }
        let host = Rc::new(ProjectHost {
            project: cx.weak_entity(),
        });
        let manager: Entity<KotlinProjectManager> =
            cx.new(|cx| KotlinProjectManager::new(host, dirs.clone(), cx));
        let project_id = cx.entity_id();
        cx.default_global::<KotlinRegistry>()
            .register_project(project_id, manager.downgrade());

        for worktree in project.worktrees(cx).collect::<Vec<_>>() {
            add_worktree(&manager, &worktree, cx);
        }

        let lsp_store = project.lsp_store();
        cx.subscribe(&lsp_store, {
            let manager = manager.clone();
            move |_, lsp_store, event, cx| {
                manager.update(cx, |manager, cx| {
                    manager.handle_lsp_event(&lsp_store, event, cx)
                });
            }
        })
        .detach();

        cx.subscribe(
            &project.worktree_store(),
            move |_, _, event, cx| match event {
                WorktreeStoreEvent::WorktreeAdded(worktree) => add_worktree(&manager, worktree, cx),
                WorktreeStoreEvent::WorktreeRemoved(_, worktree_id) => {
                    manager.update(cx, |manager, cx| manager.remove_worktree(*worktree_id, cx));
                }
                _ => {}
            },
        )
        .detach();
    })
    .detach();
}

fn add_worktree(
    manager: &Entity<KotlinProjectManager>,
    worktree: &Entity<project::Worktree>,
    cx: &mut App,
) {
    let (id, root, local) = {
        let worktree = worktree.read(cx);
        (worktree.id(), worktree.abs_path(), worktree.is_local())
    };
    if local {
        manager.update(cx, |manager, cx| {
            manager.add_worktree(id, root.to_path_buf(), cx)
        });
    }
}

/// The id of a worktree's fallback notice: a new fallback replaces the old notice, never stacks.
struct FallbackNotice;

/// The notification id of the fallback notice of a worktree.
pub fn fallback_notice_id(worktree_id: WorktreeId) -> NotificationId {
    NotificationId::composite::<FallbackNotice>(worktree_id.to_usize())
}

/// The id of a worktree's rejected-trial notice: a later rejection replaces it, never stacks.
struct TrialNotice;

/// The notification id of the rejected-trial notice of a worktree.
pub fn trial_notice_id(worktree_id: WorktreeId) -> NotificationId {
    NotificationId::composite::<TrialNotice>(worktree_id.to_usize())
}

/// The id of the toast about a malformed user manifest.
struct ManifestNotice;

/// The id of the download notification: one per window, replaced rather than stacked.
struct DownloadNotice;

pub fn download_notice_id() -> NotificationId {
    NotificationId::unique::<DownloadNotice>()
}

/// Shows the one dismissible "newer server is downloading" notification of a window, with a
/// Cancel button that cancels the background download.
pub fn show_download_notice(workspace: &mut Workspace, version: &str, cx: &mut Context<Workspace>) {
    let message = format!("A newer Kotlin server (kotlin-lsp {version}) is downloading");
    workspace.show_notification(download_notice_id(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(message, cx)
                .primary_message("Cancel")
                .primary_on_click(|_, cx| {
                    if let Some(updates) = KotlinUpdates::global(cx) {
                        updates.update(cx, |updates, cx| updates.cancel_download(cx));
                    }
                })
        })
    });
}

/// Shows each fallback of a workspace's project as one toast (the fallback policy: one notice line
/// per fallback, never a modal), each rejected trial build as one toast, and each background
/// download of a newer server once per window.
/// Call after `attach_managers`, before any workspace is created.
pub fn attach_notices(cx: &mut App) {
    cx.observe_new::<Workspace>(|workspace, _window, cx| {
        // A bad user manifest is reported once: the first workspace takes the pending message.
        if let Some(pending) = cx.try_global::<ManifestNoticePending>().cloned() {
            cx.remove_global::<ManifestNoticePending>();
            workspace.show_toast(
                Toast::new(NotificationId::unique::<ManifestNotice>(), pending.0),
                cx,
            );
        }
        if let Some(updates) = KotlinUpdates::global(cx) {
            cx.subscribe(
                &updates,
                |workspace, _, event: &KotlinUpdatesEvent, cx| match event {
                    KotlinUpdatesEvent::DownloadStarted { version } => {
                        show_download_notice(workspace, version, cx)
                    }
                    KotlinUpdatesEvent::DownloadFinished { .. } => {}
                },
            )
            .detach();
        }
        let project = workspace.project().entity_id();
        let Some(manager) = cx
            .try_global::<KotlinRegistry>()
            .and_then(|registry| registry.manager_of_project(project))
        else {
            return;
        };
        cx.subscribe(
            &manager,
            |workspace, _, event: &KotlinEvent, cx| match event {
                KotlinEvent::FellBack {
                    worktree_id,
                    message,
                } => workspace.show_toast(
                    Toast::new(fallback_notice_id(*worktree_id), message.clone()),
                    cx,
                ),
                KotlinEvent::TrialFailed {
                    worktree_id,
                    message,
                } => workspace.show_toast(
                    Toast::new(trial_notice_id(*worktree_id), message.clone()),
                    cx,
                ),
            },
        )
        .detach();
    })
    .detach();
}

#[cfg(test)]
mod notice_tests {
    use std::path::Path;

    use gpui::TestAppContext;
    use project::Project;

    use super::*;

    async fn window(cx: &mut TestAppContext) -> gpui::WindowHandle<Workspace> {
        let app_state = cx.update(workspace::AppState::test);
        let project = Project::test(app_state.fs.clone(), [] as [&Path; 0], cx).await;
        cx.add_window(|window, cx| Workspace::test_new(project, window, cx))
    }

    #[gpui::test]
    async fn test_the_download_notice_shows_once_per_window_and_can_be_dismissed(
        cx: &mut TestAppContext,
    ) {
        let first = window(cx).await;
        let second = window(cx).await;
        for window in [first, second] {
            window
                .update(cx, |workspace, _, cx| {
                    show_download_notice(workspace, "263.3.0", cx);
                    // The same download again does not stack a second one.
                    show_download_notice(workspace, "263.3.0", cx);
                    assert_eq!(workspace.notification_ids(), [download_notice_id()]);
                })
                .unwrap();
        }
        // Dismissing it in one window leaves the other window's.
        first
            .update(cx, |workspace, _, cx| {
                workspace.dismiss_notification(&download_notice_id(), cx);
                assert!(workspace.notification_ids().is_empty());
            })
            .unwrap();
        second
            .update(cx, |workspace, _, _| {
                assert_eq!(workspace.notification_ids().len(), 1);
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_a_bad_user_manifest_is_toasted_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ManifestNoticePending("manifest ignored".to_string()));
            attach_notices(cx);
        });
        let first = window(cx).await;
        let second = window(cx).await;
        first
            .update(cx, |workspace, _, _| {
                assert_eq!(workspace.notification_ids().len(), 1);
            })
            .unwrap();
        second
            .update(cx, |workspace, _, _| {
                assert!(workspace.notification_ids().is_empty());
            })
            .unwrap();
    }
}
