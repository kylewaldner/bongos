//! Which tree a task's checks run in. The first of these that exists wins: the worktree of the
//! task's active workflow run, the tree a lane serves ([`TaskTrees::tip`]), the worktree of the
//! task's last ended run, and the `worktree` of the task's file. A task with none has no tree to
//! check.

use std::path::{Path, PathBuf};

use bongos_workflow::{RunId, WorkflowEngine};
use gpui::App;

use crate::trees::TaskTrees;

/// A tree a task's checks run in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskTree {
    pub path: PathBuf,
    /// The workflow run whose worktree this is, when it is a run's.
    pub run: Option<RunId>,
}

/// What the workflow engine knows about a task's runs, as a seam so a test needs no engine.
pub trait RunWorktrees: 'static {
    /// The active run of the task and its worktree.
    fn active(&self, task_id: &str, cx: &App) -> Option<(RunId, PathBuf)>;
    /// The run of the task that ended last and its worktree; the caller checks it still exists.
    fn last_ended(&self, task_id: &str, cx: &App) -> Option<(RunId, PathBuf)>;
}

/// [`RunWorktrees`] over the app's workflow engine; nothing while there is none.
pub struct WorkflowRunWorktrees;

impl RunWorktrees for WorkflowRunWorktrees {
    fn active(&self, task_id: &str, cx: &App) -> Option<(RunId, PathBuf)> {
        cx.try_global::<WorkflowEngine>()?
            .active_run_worktree(task_id)
    }

    fn last_ended(&self, task_id: &str, cx: &App) -> Option<(RunId, PathBuf)> {
        let engine = cx.try_global::<WorkflowEngine>()?;
        let lifecycle = engine.lifecycle();
        let active = lifecycle.active_run_id(task_id);
        lifecycle
            .all_runs()
            .into_iter()
            .rev()
            .filter(|run| Some(run) != active.as_ref())
            .filter(|run| lifecycle.task_of(run) == Some(task_id))
            .find_map(|run| {
                let path = lifecycle.worktree(&run)?.path.clone();
                Some((run, path))
            })
    }
}

/// The tree of `task_id` in repository `repo` (its own repository; `None` when the task names
/// none), or `None` when no source has one.
pub(crate) fn resolve_task_tree(
    task_id: &str,
    repo: Option<&str>,
    file_worktree: Option<&Path>,
    runs: &dyn RunWorktrees,
    trees: Option<&dyn TaskTrees>,
    cx: &App,
) -> Option<TaskTree> {
    if let Some((run, path)) = runs.active(task_id, cx) {
        return Some(TaskTree {
            path,
            run: Some(run),
        });
    }
    if let Some(path) = repo
        .zip(trees)
        .and_then(|(repo, trees)| trees.tip(task_id, repo))
    {
        return Some(TaskTree { path, run: None });
    }
    if let Some((run, path)) = runs.last_ended(task_id, cx)
        && path.is_dir()
    {
        return Some(TaskTree {
            path,
            run: Some(run),
        });
    }
    file_worktree
        .filter(|path| path.is_dir())
        .map(|path| TaskTree {
            path: path.to_path_buf(),
            run: None,
        })
}
