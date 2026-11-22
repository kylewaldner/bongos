//! Lanes for bongos, on top of the lane model: the `lanes` settings, the `lanes` definition folder
//! and the prompt check of workflow validation, the run state of lanes in a project's state store
//! with its snapshot exporter, the live and activity sources a project repository reads, the
//! `lane-run-scope` approval hook, and the lane commands (hidden while `lanes.enabled` is false).
//! Also where a lane's trees and directories go and how they are made (`locations`, `create`,
//! `prepare`), the job environment (`env`), the project profile seam and the job hooks; a job's
//! trees (`job_trees`), the reset and save of a lane's persistent tree and the merge in it
//! (`lane_tree`), the worktree roots lanes give a hand start (`roots`), and a task's trees with
//! the exclusive leases of a lane's second tree that the checks ask for (`task_trees`). The
//! dispatcher (`dispatcher`, `dispatch_job`) picks, starts and ends the lanes' jobs in one loop,
//! with the pick extensions of other features (`pick_extension`) taking part in its picks;
//! `adopt` gives a stopped or halted task to a free lane, and `accept_graph` lands the graph stamp.
//! A prepared task's first job runs its rule's staleness script first (`staleness`). Other
//! features decide what a waiting lane does through the lane wait handlers (`lane_waits`), whose
//! asks reach the Work panel through the `lanes` needs-you source.

mod accept_graph;
mod actions;
mod adopt;
mod create;
mod definitions;
mod dispatch_job;
mod dispatcher;
mod done_owners;
mod env;
mod exporter;
mod hooks;
mod host;
mod job_trees;
mod lane_control;
mod lane_tree;
mod lane_waits;
mod locations;
mod pick_extension;
mod prepare;
mod profile;
mod roots;
mod runs;
mod scope;
mod settings;
mod staleness;
mod status_doc;
mod store;
mod task_trees;

use std::sync::Arc;

use bongos_project_repo::{ActivitySources, LiveSources};
use gpui::App;

pub use accept_graph::{
    ACCEPT_SUBJECT, Accepted, STAMP_FILE, accept_graph, accept_graph_for_peer, accept_graph_now,
    read_stamp, stamp_at, stamp_path,
};
pub use actions::{
    AdoptArgs, AdoptPrompt, DROP_CHOICES, ENGINE_HOST_MESSAGE, LanesToast,
    NOTHING_TO_ADOPT_MESSAGE, OFF_MESSAGE, ONE_PROJECT_MESSAGE, WindowLanes, accepted_message,
    action_types, adopt_as_person, adopt_prompt, adoptable_tasks, adopted_message, init_palette,
    lanes, register,
};
pub use adopt::{AdoptRequest, Adopted, LeftWork};
/// The writers' participants live in the engine host, which drains them at a handover; lanes
/// and other features register theirs here.
pub use bongos_engine_host::{WriterParticipant, WriterParticipants};
pub use bongos_lanes_model::{AssignedWork, Assignment, EnvEdit, Problem, StopCause};
pub use create::{CreateError, check_root, create_below_existing, lexical};
pub use definitions::{LANES_FILE, LANES_FOLDER, prompt_lane_diagnostics, validate_folder};
pub use dispatch_job::{DROPPED_BEFORE_START, HISTORY_BY};
pub use dispatcher::{
    AheadLanding, DEFAULT_DISPATCHER, DISPATCHER_KEY, Dispatcher, DispatcherConfig,
    DispatcherRecord, JobCommit, JobDefinitionPolicy, JobEngine, JobSetup, Lanes, LanesHost,
    LanesParts, LanesStarted, NOT_STARTED_BEFORE_A_CRASH, NOTHING_READY, PROJECT_UNATTENDED,
    RecordedBound, StartLanes, StartRefusal, VerifiedCommitPolicy, dispatcher_record,
    put_dispatcher,
};
pub use done_owners::CLEARED_NO_CHECKS;
pub use env::{apply_env, second_tree_env, tip_env};
pub use exporter::{EXPORT_NAMESPACE, LanesExporter, STATE_FILE};
pub use hooks::{AFTER_TREE_CREATED, BEFORE_JOB, BeforeJob, JobEnd, JobHook, JobHookCx, JobHooks};
pub use host::{
    HOST_PREFIX, HostDispatcher, LanesPayload, ProjectLanesHost, WINDOW_PREFIX, dispatcher_running,
    ensure_window_lanes, register_host,
};
pub use job_trees::{
    CurrentJobTrees, ExtraRepositories, JobRepositories, JobRepository, JobStart, JobTrees,
    StartedTrees, forbidden_for, start_job_trees,
};
pub use lane_control::{
    BOUND_REASON, DispatcherPhase, LaneEvent, LaneEvents, LaneStatus, LaneTarget, LanesStatus,
    RunEnd,
};
pub use lane_tree::{
    Forbidden, LaneMerge, LaneMergeError, LaneMerged, LastEnd, LastRun, MergeTarget, ResetOutcome,
    ResetRequest, SAVE_EMAIL, SAVE_NAME, SAVED_REFS, SaveError, SaveTarget, Saved, add_tree,
    check_own_tree, git_tree_maker, lane_tree, merge_in_lane_tree, reset_tree, save_left_work,
};
pub use lane_waits::{
    ASK_PREFIX, LaneAsk, LaneWaitAction, LaneWaitHandler, LaneWaitHandlers, NEEDS_YOU_SOURCE,
    read_asks, register_needs_you,
};
pub use locations::{
    Fallback, LaneLocation, LaneLocations, Layer, LocationInputs, Place, RepositoryPlace,
    binding_of, check_lanes, check_overlap, resolve_lane, table_lane_ids, tree_name, tree_slug,
};
pub use pick_extension::{
    ExtensionPick, JobChange, LaneAfterJob, PickExtension, PickExtensions, instance_room,
};
pub use prepare::{
    FreeSpace, MakeTree, PrepareRequest, PreparedLane, TreeRequest, check_floors, prepare_lane,
    system_free_space, wait_lane,
};
pub use profile::{LaneProfileSource, LaneProfiles};
pub use roots::{LaneRootResolver, LaneTables};
pub use runs::{LaneActivity, LaneLiveSource, LaneRuns, OwnedScope, SOURCE_NAME};
pub use scope::LaneRunScope;
pub use settings::{LanesHostSettings, LanesOnInterruption, LanesOnLastWindowClose, LanesSettings};
pub use staleness::{
    PinnedTree, Staleness, StalenessCheck, run_staleness_script, staleness_answer,
    staleness_program,
};
pub use status_doc::{STATUS_SCHEMA, StatusFacts, status_doc};
pub use store::{
    LANE_PREFIX, LANES_NAMESPACE, OWNER_PREFIX, delete_lane, delete_owner, put_lane, put_owner,
    read_state, read_state_in, write_state,
};
pub use task_trees::{
    LaneLease, LaneTaskTrees, LaneTreeSetup, OwedTasks, ProcessEnv, SecondTreeLeases, TopLevelOf,
    TreeHold, no_top_level, open_projects_owed, process_env,
};

/// Registers the settings, the `lanes` definition folder and the prompt check, the `lanes` state
/// exporter, the live and activity sources, and the `lane-run-scope` approval hook, all over one
/// [`LaneRuns`] kept as a global, the lanes' [`LaneTaskTrees`] as the checks' tree provider, and
/// the `lanes` needs-you source.
/// Nothing here starts a lane; while `lanes.enabled` is false no lanes state is known, so the
/// sources and the hook say nothing and no tree is lent.
pub fn init(cx: &mut App) {
    use ::settings::Settings as _;
    LanesSettings::register(cx);
    definitions::register(cx);
    LanesExporter::register();
    let runs = LaneRuns::default();
    LiveSources::register(Arc::new(LaneLiveSource { runs: runs.clone() }), cx);
    ActivitySources::global().register(Arc::new(LaneActivity { runs: runs.clone() }));
    LaneRunScope::register(runs.clone(), cx);
    cx.set_global(runs.clone());
    bongos_work::WorktreeRootResolvers::register(Arc::new(LaneRootResolver), cx);
    let trees = LaneTaskTrees::new(
        runs,
        CurrentJobTrees::global(cx),
        no_top_level(),
        process_env(),
    );
    trees.set_owed(open_projects_owed(bongos_work::OpenProjectRepos::global(
        cx,
    )));
    trees.register(cx);
    register_needs_you(cx);
    register_host(cx);
}

#[cfg(test)]
mod accept_graph_tests;
#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod create_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod dispatcher_tests;
#[cfg(test)]
mod done_owners_tests;
#[cfg(test)]
mod env_tests;
#[cfg(test)]
mod exporter_tests;
#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod job_trees_tests;
#[cfg(test)]
mod lane_tree_tests;
#[cfg(test)]
mod locations_tests;
#[cfg(test)]
mod prepare_tests;
#[cfg(test)]
mod roots_tests;
#[cfg(test)]
mod runs_tests;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod staleness_tests;
#[cfg(test)]
mod task_trees_tests;
#[cfg(test)]
mod test_support;
