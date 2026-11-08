//! `task: open worktree`, `task: open pr` and `task: copy path`: a task's worktree, PR and path
//! reached by name.

use std::path::PathBuf;

use bongos_work_model::store::{Task, WorkTree};
use gpui::{ClipboardItem, Context, Window};
use workspace::Workspace;

use crate::actions::task;
use crate::commands::{ready, show_toast, with_task};
use crate::modals;
use crate::worktrees::{expand_home, pull_request_urls};

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &task::OpenWorktree, window, cx| open_worktree(w, window, cx))
        .register_action(|w, _: &task::OpenPr, window, cx| open_pr(w, window, cx))
        .register_action(|w, _: &task::CopyPath, window, cx| copy_path(w, window, cx));
}

fn worktree(task: &Task) -> Option<&str> {
    task.file.as_ref()?.worktree.as_deref()
}

fn pull_requests(task: &Task) -> Vec<String> {
    task.file
        .as_ref()
        .map(pull_request_urls)
        .unwrap_or_default()
}

/// The path `task: copy path` copies for the task `id`: its worktree, else its directory.
pub(crate) fn task_path(tree: &WorkTree, id: &str) -> Option<PathBuf> {
    let task = tree.task_by_id(id)?;
    Some(worktree(task).map_or_else(|| task.dir.clone(), expand_home))
}

fn open_worktree(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Open the worktree of which task?",
        "No task has a worktree; task: start makes one",
        |task, _| worktree(task).is_some(),
        Box::new(|id, workspace, _, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let Some(recorded) = tree.task_by_id(&id).and_then(worktree) else {
                return;
            };
            let path = expand_home(recorded);
            if !path.is_dir() {
                show_toast(workspace, format!("The worktree {recorded} is gone"), cx);
                return;
            }
            let added = workspace.project().update(cx, |project, cx| {
                project.find_or_create_worktree(path, true, cx)
            });
            cx.spawn(async move |workspace, cx| {
                if let Err(error) = added.await {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_toast(
                                workspace,
                                format!("Could not open the worktree: {error}"),
                                cx,
                            )
                        })
                        .ok();
                }
            })
            .detach();
        }),
    );
}

fn open_pr(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Open the PR of which task?",
        "No task has a PR as its result",
        |task, _| !pull_requests(task).is_empty(),
        Box::new(|id, workspace, window, cx| {
            let Some(tree) = ready(workspace, cx) else {
                return;
            };
            let urls = tree.task_by_id(&id).map(pull_requests).unwrap_or_default();
            match urls.as_slice() {
                [] => {}
                [url] => cx.open_url(url),
                _ => modals::pick(
                    workspace,
                    "Open which PR?",
                    urls.clone(),
                    0,
                    Box::new(move |index, _, _, cx| cx.open_url(&urls[index])),
                    window,
                    cx,
                ),
            }
        }),
    );
}

fn copy_path(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_task(
        workspace,
        window,
        cx,
        "Copy the path of which task?",
        "There is no task",
        |_, _| true,
        Box::new(|id, workspace, _, cx| {
            let Some(path) = ready(workspace, cx).and_then(|tree| task_path(&tree, &id)) else {
                return;
            };
            let path = path.to_string_lossy().into_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
            show_toast(workspace, format!("Copied {path}"), cx);
        }),
    );
}
