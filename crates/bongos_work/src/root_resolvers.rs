//! Worktree roots that other features decide: a registered [`WorktreeRootResolver`] answers the
//! root of a project's repository before the user's worktree settings do. With none registered,
//! or none answering, `task: start` uses the settings and the central directory as it always did.

use std::sync::Arc;

use bongos_work_model::worktree_path::Source;
use gpui::{App, Global};

/// One feature's worktree roots.
pub trait WorktreeRootResolver: Send + Sync {
    /// The worktree root template (with `{repo}` and `{slug}`) of `repo`'s worktrees in the
    /// project `project_id`, with the layer that set it; `None` leaves the decision to the next
    /// resolver and then to the settings.
    fn root(&self, project_id: &str, repo: &str, cx: &App) -> Option<(String, Source)>;
}

/// The registered resolvers, asked in the order they were registered.
#[derive(Default)]
pub struct WorktreeRootResolvers {
    resolvers: Vec<Arc<dyn WorktreeRootResolver>>,
}

impl Global for WorktreeRootResolvers {}

impl WorktreeRootResolvers {
    pub fn register(resolver: Arc<dyn WorktreeRootResolver>, cx: &mut App) {
        cx.default_global::<Self>().resolvers.push(resolver);
    }

    /// The first answer of a registered resolver for `repo` in `project_id`.
    pub fn root(project_id: &str, repo: &str, cx: &App) -> Option<(String, Source)> {
        cx.try_global::<Self>()?
            .resolvers
            .iter()
            .find_map(|resolver| resolver.root(project_id, repo, cx))
    }
}
