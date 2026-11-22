//! The project profile seam: what this machine says about a project's lanes (counts, locations,
//! environment edits, disk floors and checks of its drives). A later feature registers a source
//! with [`LaneProfiles::register`]; with none registered there is no profile, and every lane uses
//! the project's own values and the default locations, with no verify and no floor.

use std::path::PathBuf;
use std::sync::Arc;

use bongos_lanes_model::{EnvEdit, LaneId, Problem};
use gpui::{App, BorrowAppContext as _, Global, Task};

/// What a project profile answers. Every method takes the project id; each default answers "no
/// profile here": `None`, an empty list or `Ok`.
///
/// Location names: `tree:<repo>` is the lane tree of a repository, `tree:<repo>:base` its second
/// tree, `<name>` a named directory of the binding and `<name>-base` a second-tree directory
/// listed in `base_tree.dirs`.
pub trait LaneProfileSource {
    /// The profile's name, shown as the layer that set a location.
    fn name(&self, _project: &str) -> Option<String> {
        None
    }

    /// The count of lanes of `kind` on this machine, overriding the lane table's.
    fn count(&self, _project: &str, _kind: &str) -> Option<u32> {
        None
    }

    /// The path of the location `name` of `lane`.
    fn location(&self, _project: &str, _lane: &LaneId, _name: &str) -> Option<PathBuf> {
        None
    }

    /// The worktree root template (with `{repo}` and `{slug}`) of `repo`'s trees.
    fn worktree_root(&self, _project: &str, _repo: &str) -> Option<String> {
        None
    }

    /// Edits to a lane job's environment, applied after the binding's own `env`.
    fn env(&self, _project: &str, _lane: &LaneId) -> Vec<EnvEdit> {
        Vec::new()
    }

    /// The least free space, in bytes, the disk of `location` must keep before a job.
    fn floor(&self, _project: &str, _lane: &LaneId, _location: &str) -> Option<u64> {
        None
    }

    /// Checks the machine before the dispatcher starts.
    fn verify_dispatcher(&self, _project: &str, _cx: &mut App) -> Task<Result<(), Vec<Problem>>> {
        Task::ready(Ok(()))
    }

    /// Checks the machine for one lane (its drives mounted, say) before anything of it is made.
    fn verify_lane(
        &self,
        _project: &str,
        _lane: &LaneId,
        _cx: &mut App,
    ) -> Task<Result<(), Vec<Problem>>> {
        Task::ready(Ok(()))
    }
}

/// The registered project profile source, if any.
#[derive(Default)]
pub struct LaneProfiles {
    source: Option<Arc<dyn LaneProfileSource>>,
}

impl Global for LaneProfiles {}

impl LaneProfiles {
    /// Makes `source` the project profile; a later registration replaces it.
    pub fn register(source: Arc<dyn LaneProfileSource>, cx: &mut App) {
        cx.default_global::<Self>().source = Some(source);
    }

    /// Tells the running dispatchers that the profile's answers changed (a lane count, say); they
    /// apply the new counts at once, each lane at its job boundary.
    pub fn changed(cx: &mut App) {
        if cx.has_global::<Self>() {
            cx.update_global::<Self, _>(|_, _| {});
        }
    }

    /// The registered source, or `None` when there is no profile.
    pub fn source(cx: &App) -> Option<Arc<dyn LaneProfileSource>> {
        cx.try_global::<Self>()
            .and_then(|profiles| profiles.source.clone())
    }
}
