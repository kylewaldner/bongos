//! The checks hook on a workflow run's completion. A run that completes its task's todo marks the
//! task in that write when it finishes the task; the run ends only after its completion hooks,
//! so this hook takes the done decision of the task's project while the run's worktree still
//! exists, with the task's tree pinned to that worktree. The same decision fills the tree keys of
//! the `checks.last` rows that name it, so a group's last child is keyed on the tree it finished
//! on.
//!
//! The hook writes nothing of its own: whatever the decision cannot do leaves the task held by
//! its marker, which only a done record clears. It is registered at start and does nothing while
//! `task_checks.enabled` is false, or while this process does not hold the project's state writer.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bongos_workflow::{CompletionHook, CompletionHooks, RunId};
use gpui::{App, AsyncApp, Context, Task, WeakEntity};

use crate::engine::ChecksEngine;
use crate::task_tree::TaskTree;

/// The [`CompletionHook`] of task checks.
pub struct ChecksCompletionHook;

impl CompletionHook for ChecksCompletionHook {
    fn after_complete(
        &self,
        task_id: &str,
        run_id: &RunId,
        worktree: &Path,
        cx: &mut App,
    ) -> Task<()> {
        let Some(engine) = ChecksEngine::global(cx) else {
            return Task::ready(());
        };
        engine.update(cx, |engine, cx| {
            engine.decide_completed(task_id, run_id, worktree.to_path_buf(), cx)
        })
    }
}

/// Registers [`ChecksCompletionHook`].
pub(crate) fn register(cx: &mut App) {
    CompletionHooks::register(Rc::new(ChecksCompletionHook), cx);
}

impl ChecksEngine {
    /// Takes a done decision of `task_id`'s project with the task's tree being `worktree`, the
    /// worktree of the run `run` that just completed its todo, and resolves once it has ended.
    /// Resolves at once while this process does not decide or does not know the task.
    pub fn decide_completed(
        &mut self,
        task_id: &str,
        run: &RunId,
        worktree: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        if !self.deciding(cx) {
            return Task::ready(());
        }
        let Some(task) = self.task(task_id, cx) else {
            return Task::ready(());
        };
        let project = task.project.location().id.clone();
        // Followed already in the usual case; a no-op when this process does not hold the writer.
        self.follow_project(&task.project, cx);
        let Some(decided) = self.decider.decide_and_wait(&project) else {
            return Task::ready(());
        };
        self.completing.insert(
            task_id.to_string(),
            TaskTree {
                path: worktree,
                run: Some(run.clone()),
            },
        );
        let pin = CompletingPin {
            engine: cx.weak_entity(),
            task_id: task_id.to_string(),
            run: run.clone(),
            cx: Some(cx.to_async()),
        };
        cx.spawn(async move |this, cx| {
            let mut pin = pin;
            decided.await.ok();
            this.update(cx, |engine, _| {
                engine.unpin_completing(&pin.task_id, &pin.run)
            })
            .ok();
            pin.cx = None;
        })
    }

    /// Forgets the worktree `run` pinned for `task_id`, unless another run pinned it since.
    fn unpin_completing(&mut self, task_id: &str, run: &RunId) {
        if self
            .completing
            .get(task_id)
            .is_some_and(|tree| tree.run.as_ref() == Some(run))
        {
            self.completing.remove(task_id);
        }
    }
}

/// The pin of a completing run's worktree, held by the task [`ChecksEngine::decide_completed`]
/// returns. Dropped before its decision ended (the run's end gave up on it), it unpins the
/// worktree on the next turn of the foreground executor, since the app may be borrowed then.
struct CompletingPin {
    engine: WeakEntity<ChecksEngine>,
    task_id: String,
    run: RunId,
    /// `None` once the decision ended and unpinned the worktree itself.
    cx: Option<AsyncApp>,
}

impl Drop for CompletingPin {
    fn drop(&mut self) {
        let Some(mut cx) = self.cx.take() else {
            return;
        };
        let engine = self.engine.clone();
        let task_id = std::mem::take(&mut self.task_id);
        let run = std::mem::take(&mut self.run);
        cx.foreground_executor()
            .clone()
            .spawn(async move {
                engine
                    .update(&mut cx, |engine, _| engine.unpin_completing(&task_id, &run))
                    .ok();
            })
            .detach();
    }
}
