//! The worktree roots lanes decide for a hand-started task while `lanes.enabled` is on: the
//! project profile's `worktree_root` (layer `Profile`), else the project's `lanes.toml`
//! `[locations]` (layer `Project`). Neither falls back to the central directory. With lanes off it
//! answers nothing, so a hand start uses the user's worktree settings as before.

use std::collections::BTreeMap;
use std::sync::Arc;

use bongos_lanes_model::LaneTable;
use bongos_work::WorktreeRootResolver;
use bongos_work_model::worktree_path::Source;
use gpui::{App, Global};
use settings::Settings as _;

use crate::profile::LaneProfiles;
use crate::settings::LanesSettings;

/// The lane table of each project, as last read from its definitions, for the features that need
/// it outside a job start.
#[derive(Default)]
pub struct LaneTables {
    tables: BTreeMap<String, Arc<LaneTable>>,
}

impl Global for LaneTables {}

impl LaneTables {
    pub fn set(project_id: impl Into<String>, table: Arc<LaneTable>, cx: &mut App) {
        cx.default_global::<Self>()
            .tables
            .insert(project_id.into(), table);
    }

    pub fn get(project_id: &str, cx: &App) -> Option<Arc<LaneTable>> {
        cx.try_global::<Self>()?.tables.get(project_id).cloned()
    }
}

/// The resolver registered with `bongos_work` by [`crate::init`].
pub struct LaneRootResolver;

impl WorktreeRootResolver for LaneRootResolver {
    fn root(&self, project_id: &str, repo: &str, cx: &App) -> Option<(String, Source)> {
        if !LanesSettings::get_global(cx).enabled {
            return None;
        }
        if let Some(profile) = LaneProfiles::source(cx)
            && let Some(root) = profile.worktree_root(project_id, repo)
        {
            let name = profile
                .name(project_id)
                .unwrap_or_else(|| "(unnamed)".to_string());
            return Some((root, Source::Profile { name }));
        }
        let table = LaneTables::get(project_id, cx)?;
        let root = table.locations.worktree_root_of(repo)?;
        Some((root.value.clone(), Source::Project))
    }
}
