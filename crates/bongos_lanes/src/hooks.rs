//! Job hooks: points in a lane job's start where other features run their own steps. With none
//! registered nothing runs.

use std::path::Path;
use std::sync::Arc;

use bongos_lanes_model::LaneId;
use gpui::{App, Global, Task};

/// The hook point named in a wait reason when [`JobHook::after_tree_created`] fails.
pub const AFTER_TREE_CREATED: &str = "after_tree_created";

/// One feature's hooks. Each default does nothing.
pub trait JobHook {
    /// Runs once after a lane tree (a tip or a second tree) of `repo` is made at `path` for the
    /// first time, before the job uses it. The lane waits for the task; an error leaves the lane
    /// waiting with its message.
    fn after_tree_created(
        &self,
        _lane: &LaneId,
        _repo: &str,
        _path: &Path,
        _cx: &mut App,
    ) -> Task<Result<(), String>> {
        Task::ready(Ok(()))
    }
}

/// The registered job hooks, called in the order they were registered.
#[derive(Default)]
pub struct JobHooks {
    hooks: Vec<Arc<dyn JobHook>>,
}

impl Global for JobHooks {}

impl JobHooks {
    pub fn register(hook: Arc<dyn JobHook>, cx: &mut App) {
        cx.default_global::<Self>().hooks.push(hook);
    }

    /// Every registered hook.
    pub fn hooks(cx: &App) -> Vec<Arc<dyn JobHook>> {
        cx.try_global::<Self>()
            .map(|hooks| hooks.hooks.clone())
            .unwrap_or_default()
    }
}
