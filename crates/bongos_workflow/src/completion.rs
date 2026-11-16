//! What the end of a successful run waits for after it completed the run's todo: every
//! [`CompletionHook`] registered with [`CompletionHooks::register`] is called with the task, the
//! run and the run's worktree, and the run ends only once each hook's task resolved. The run's
//! worktree therefore still exists while a hook looks at it.
//!
//! A hook cannot refuse the completion: the todo is already completed when it is called, and
//! whatever it fails to do it must leave the task as the completion left it. With no hook
//! registered the end of a run is as it was.

use std::path::Path;
use std::rc::Rc;

use gpui::{App, Global, Task};

use crate::lifecycle::RunId;

/// Called after a successful run completed its task's todo, before the run ends.
pub trait CompletionHook: 'static {
    /// Runs after `run_id` completed its todo on `task_id`; `worktree` is the run's worktree,
    /// which stays until the returned task resolves.
    fn after_complete(
        &self,
        task_id: &str,
        run_id: &RunId,
        worktree: &Path,
        cx: &mut App,
    ) -> Task<()>;
}

/// The registered [`CompletionHook`]s, in registration order.
#[derive(Default)]
pub struct CompletionHooks(Vec<Rc<dyn CompletionHook>>);

impl Global for CompletionHooks {}

impl CompletionHooks {
    /// Adds `hook` for the life of the app.
    pub fn register(hook: Rc<dyn CompletionHook>, cx: &mut App) {
        cx.default_global::<Self>().0.push(hook);
    }

    /// Calls every hook and resolves once all of their tasks have. With none registered it
    /// resolves at once.
    pub fn after_complete(task_id: &str, run_id: &RunId, worktree: &Path, cx: &mut App) -> Task<()> {
        let hooks: Vec<Rc<dyn CompletionHook>> = cx
            .try_global::<Self>()
            .map(|hooks| hooks.0.clone())
            .unwrap_or_default();
        if hooks.is_empty() {
            return Task::ready(());
        }
        let tasks: Vec<Task<()>> = hooks
            .iter()
            .map(|hook| hook.after_complete(task_id, run_id, worktree, cx))
            .collect();
        cx.spawn(async move |_| {
            futures::future::join_all(tasks).await;
        })
    }
}
