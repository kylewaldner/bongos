//! Connects `KotlinProjectManager` to real projects: one manager per local project, fed by the
//! project's `LspStore` and worktrees, restarting through the store.

use std::rc::Rc;

use collections::HashSet;
use gpui::{App, AppContext as _, Entity, WeakEntity};
use lsp::{LanguageServerName, LanguageServerSelector};
use project::{Project, worktree_store::WorktreeStoreEvent};
use settings::WorktreeId;

use crate::{
    KotlinDirs,
    manager::{ADAPTER_NAME, KotlinHost, KotlinProjectManager},
};

const KOTLIN_LANGUAGE: &str = "Kotlin";

struct ProjectHost {
    project: WeakEntity<Project>,
}

impl KotlinHost for ProjectHost {
    fn restart(&self, worktree_id: WorktreeId, cx: &mut App) {
        let Some(project) = self.project.upgrade() else {
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
        // With no Kotlin buffer open there is no server to restart: the next buffer starts the
        // effective server.
        if buffers.is_empty() {
            return;
        }
        let only: HashSet<LanguageServerSelector> =
            HashSet::from_iter([LanguageServerSelector::Name(
                LanguageServerName::new_static(ADAPTER_NAME),
            )]);
        project.read(cx).lsp_store().update(cx, |lsp_store, cx| {
            lsp_store.restart_language_servers_for_buffers(buffers, only, true, cx);
        });
    }
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
