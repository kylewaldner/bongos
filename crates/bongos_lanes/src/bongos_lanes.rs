//! Lanes for bongos, on top of the lane model: the `lanes` settings, the `lanes` definition folder
//! and the prompt check of workflow validation, the run state of lanes in a project's state store
//! with its snapshot exporter, the live and activity sources a project repository reads, the
//! `lane-run-scope` approval hook, and the lane commands (hidden while `lanes.enabled` is false).

mod actions;
mod definitions;
mod exporter;
mod runs;
mod scope;
mod settings;
mod store;

use std::sync::Arc;

use bongos_project_repo::{ActivitySources, LiveSources};
use gpui::App;

pub use actions::{
    ENGINE_HOST_MESSAGE, LanesToast, OFF_MESSAGE, action_types, init_palette, lanes, register,
};
pub use definitions::{LANES_FILE, LANES_FOLDER, prompt_lane_diagnostics, validate_folder};
pub use exporter::{EXPORT_NAMESPACE, LanesExporter, STATE_FILE};
pub use runs::{LaneActivity, LaneLiveSource, LaneRuns, OwnedScope, SOURCE_NAME};
pub use scope::LaneRunScope;
pub use settings::{LanesHostSettings, LanesOnInterruption, LanesOnLastWindowClose, LanesSettings};
pub use store::{
    LANE_PREFIX, LANES_NAMESPACE, OWNER_PREFIX, put_lane, put_owner, read_state, write_state,
};

/// Registers the settings, the `lanes` definition folder and the prompt check, the `lanes` state
/// exporter, the live and activity sources, and the `lane-run-scope` approval hook, all over one
/// [`LaneRuns`] kept as a global. Nothing here starts a lane; while `lanes.enabled` is false no
/// lanes state is known, so the sources and the hook say nothing.
pub fn init(cx: &mut App) {
    use ::settings::Settings as _;
    LanesSettings::register(cx);
    definitions::register(cx);
    LanesExporter::register();
    let runs = LaneRuns::default();
    LiveSources::register(Arc::new(LaneLiveSource { runs: runs.clone() }), cx);
    ActivitySources::global().register(Arc::new(LaneActivity { runs: runs.clone() }));
    LaneRunScope::register(runs.clone(), cx);
    cx.set_global(runs);
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod exporter_tests;
#[cfg(test)]
mod runs_tests;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod test_support;
