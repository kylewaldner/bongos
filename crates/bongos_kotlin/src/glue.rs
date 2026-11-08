//! Connects `KotlinProjectManager` to real projects: one manager per local project, fed by the
//! project's `LspStore` and worktrees, restarting through the store.

use std::rc::Rc;

use gpui::{App, AppContext as _, Entity, WeakEntity};
use lsp::{LanguageServerId, LanguageServerName};
use project::{Project, worktree_store::WorktreeStoreEvent};
use settings::WorktreeId;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::{
    KotlinDirs,
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

/// Shows each fallback of a workspace's project as one toast (the fallback policy: one notice line
/// per fallback, never a modal). Call after `attach_managers`, before any workspace is created.
pub fn attach_notices(cx: &mut App) {
    cx.observe_new::<Workspace>(|workspace, _window, cx| {
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
            },
        )
        .detach();
    })
    .detach();
}
