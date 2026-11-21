//! Task checks: scripts and commands that must pass before a task counts as done. This holds
//! the settings, the commands (hidden while `task_checks.enabled` is false), the evidence store
//! over the project's state store, the exporter of that state, and the validation of check
//! declarations in a project's definitions, the running of one check process, a check run
//! with its scope, parts, checkpoints, pool waits and no-verdict reruns, the pool seam, and the
//! engine that runs a task's checks and the commands that drive it, its live rows in the state
//! store, and the `checks` host command that reaches the engine from a process without the
//! state writer, the base trees of parameterized runs, the owed markers of finished tasks, the
//! done hold they cause, and the decider that writes the done record of a leaf whose checks
//! pass on its tree, and of a group whose closing checks pass on its last child's tree, and the
//! hook that takes that decision on a workflow run's worktree when the run completes its todo,
//! the Checks section of a task's details tab with the output and edit views of its checks, the
//! workflow step that runs checks, and the checks of a landing: the `check` kind of
//! `[[landing.checks]]`, run by the engine with master's copy of its scripts, and the approval
//! hook over loosened checks of started tasks.

mod actions;
mod base_tree;
mod changed_files;
pub mod command_logic;
mod completion;
pub mod decider;
mod definitions;
mod edit;
pub mod engine;
mod evidence_store;
mod exporter;
pub mod group_decider;
pub mod holds;
pub mod host;
pub mod landing;
pub mod live;
mod output;
mod output_view;
pub mod owed;
mod pin;
pub mod pools;
pub mod process;
mod retained;
mod retention;
pub mod run;
mod section_source;
mod settings;
pub mod task_git;
mod task_source;
mod task_tree;
mod tree_key;
mod trees;
mod work_tree;
pub mod workflow_node;

pub use actions::{ChecksToast, action_types, init_palette, register, task};
pub use changed_files::{ChangedFiles, ChangedFilesError, ChangedSet, changed_files};
pub use completion::ChecksCompletionHook;
pub use decider::{CHECKING_TREE, CLOSING_CHECKS, NO_TREE, OPEN_CHILDREN, missing_reason};
pub use definitions::{CHECKS_FOLDER, CHECKS_VALID, ChecksValid, validate_folder};
pub use engine::{
    CheckResult, ChecksEngine, ChecksEvent, ChecksStatus, Refusal, RunChecksRequest, RunOutcome,
    RunReport,
};
pub use evidence_store::{
    DONE_NAMESPACE, DoneRecord, EVIDENCE_NAMESPACE, LANDING_PREFIX, LAST_NAMESPACE, LIVE_NAMESPACE,
    OWED_NAMESPACE, SEQ_KEY, SEQ_NAMESPACE, delete_done, delete_record, done_record,
    done_record_in, evidence_key, evidence_prefix, landing_key, landing_prefix, next_generation,
    put_done, record_key, scan_records, scan_records_in, store_record, task_prefix,
};
pub use exporter::{ChecksExporter, EXPORT_NAMESPACE};
pub use holds::{
    CHECKING, CHECKS_CHIPS_SOURCE, CHECKS_HOLD_SOURCE, ChecksChips, ChecksHoldSource, ChecksHolds,
    read_hold_rows, read_holds,
};
pub use host::{
    CHECKS_NAMESPACE, ChecksAccess, ChecksCommand, ChecksForwarder, ChecksForwarderRegistry,
    ChecksReply, checks_access,
};
pub use landing::{
    CHECKS_LOOSENING, CHECKS_OFF, ChecksLoosening, LANDING_KIND, LandingJob, MOVING_TO_HOST,
    NO_PROJECT_ID, TaskCheckLanding, parse_landing_check,
};
pub use live::{HoldRow, LiveRow, clear_live, hold_key, live_rows, part_key};
pub use output::{OutputSummary, cut_marker};
pub use owed::{
    CHECKS_OWED_HOOK, ChecksOwedHook, LastChild, LastRow, OwedMarker, last_row_in, marker_in,
    put_last, put_marker,
};
pub use pin::{Pin, PinPathError, master_definitions};
pub use pools::{
    CheckClock, CheckPools, CheckPoolsRegistry, ExecutorClock, Holder, PoolLease, PoolLeaseHandle,
    PoolRequest, PoolWait, WaitStatus,
};
pub use process::{PartOutcome, PartScope, PartSpec, PartStart, ProcessRole, StopHandle, run_part};
pub use run::{CheckRun, LiveState, RunTree};
pub use settings::TaskChecksSettings;
pub use task_source::{TaskChecks, TaskSource, WorkTaskSource};
pub use task_tree::{RunWorktrees, TaskTree, WorkflowRunWorktrees};
pub use tree_key::{TreeError, TreeKey, combine_trees, key_inputs_digest, tree_key};
pub use trees::{EnvBase, SecondTree, SecondTreeLease, TaskTrees, TaskTreesRegistry};
pub use workflow_node::{ChecksNode, ChecksNodeStatus, NodeJob, node_result};

/// Registers the settings, the `checks-owed` state hook, the `checks` done hold source, the
/// `checks` state exporter, the `checks` definition folder, the `checks-valid` landing check, the
/// global [`ChecksEngine`], its workflow completion hook, the built-in behind `use = "checks"`
/// workflow nodes with its run view status, the `check` landing kind whose jobs the engine
/// receives, and the approval hook over loosened checks (inert while the setting is off).
/// Runs before any project opens: the first state writer seals the registry of state hooks.
pub fn init(cx: &mut gpui::App) {
    use ::settings::Settings as _;
    TaskChecksSettings::register(cx);
    owed::register_hook(cx);
    holds::init(cx);
    exporter::ChecksExporter::register();
    definitions::register(cx);
    let engine = ChecksEngine::install(ChecksEngine::for_app(cx), cx);
    decider::init(engine.clone(), cx);
    section_source::init(&engine, cx);
    completion::register(cx);
    landing::register(&engine, cx);
    workflow_node::ChecksNode::register(&engine, cx);
    // What a crash left in the scratch folder goes at load. The parts it left running go as the
    // decider (started above) follows each project's store: `ChecksEngine::follow_project`.
    if let Ok(git) = bongos_project_repo::Git::locate() {
        let git = task_git::TaskGit::new(git, std::path::PathBuf::new());
        engine
            .update(cx, |engine, cx| engine.sweep_scratch(git, cx))
            .detach();
    }
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod base_run_tests;
#[cfg(test)]
mod changed_files_tests;
#[cfg(test)]
mod decider_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod edit_checks_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod evidence_store_tests;
#[cfg(test)]
mod exporter_tests;
#[cfg(test)]
mod group_decider_tests;
#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod landing_tests;
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod owed_tests;
#[cfg(test)]
mod pools_tests;
#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod retention_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod task_git_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tree_key_tests;
#[cfg(test)]
mod workflow_node_tests;
