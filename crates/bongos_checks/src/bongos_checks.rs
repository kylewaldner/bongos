//! Task checks: scripts and commands that must pass before a task counts as done. This holds
//! the settings, the commands (hidden while `task_checks.enabled` is false), the evidence store
//! over the project's state store, the exporter of that state, and the validation of check
//! declarations in a project's definitions, the running of one check process, a check run
//! with its scope, parts, checkpoints, pool waits and no-verdict reruns, the pool seam, and the
//! engine that runs a task's checks and the commands that drive it.

mod actions;
mod changed_files;
pub mod command_logic;
mod definitions;
pub mod engine;
mod evidence_store;
mod exporter;
mod output;
mod pin;
pub mod pools;
pub mod process;
pub mod run;
mod settings;
mod task_source;
mod task_tree;
mod tree_key;
mod trees;
mod work_tree;

pub use actions::{ChecksToast, action_types, init_palette, register, task};
pub use changed_files::{ChangedFiles, ChangedFilesError, ChangedSet, changed_files};
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
pub use output::{OutputSummary, cut_marker};
pub use pin::{Pin, PinPathError, master_definitions};
pub use pools::{
    CheckClock, CheckPools, CheckPoolsRegistry, ExecutorClock, Holder, PoolLease, PoolLeaseHandle,
    PoolRequest, PoolWait, WaitStatus,
};
pub use process::{PartOutcome, PartScope, PartSpec, PartStart, ProcessRole, StopHandle, run_part};
pub use run::{CheckRun, LiveState};
pub use settings::TaskChecksSettings;
pub use task_source::{TaskChecks, TaskSource, WorkTaskSource};
pub use task_tree::{RunWorktrees, TaskTree, WorkflowRunWorktrees};
pub use tree_key::{TreeError, TreeKey, combine_trees, key_inputs_digest, tree_key};
pub use trees::{EnvBase, SecondTree, SecondTreeLease, TaskTrees, TaskTreesRegistry};

/// Registers the settings, the `checks` state exporter, the `checks` definition folder, the
/// `checks-valid` landing check and the global [`ChecksEngine`].
pub fn init(cx: &mut gpui::App) {
    use ::settings::Settings as _;
    TaskChecksSettings::register(cx);
    exporter::ChecksExporter::register();
    definitions::register(cx);
    ChecksEngine::install(ChecksEngine::for_app(cx), cx);
}

#[cfg(test)]
mod actions_tests;
#[cfg(test)]
mod changed_files_tests;
#[cfg(test)]
mod definitions_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod evidence_store_tests;
#[cfg(test)]
mod exporter_tests;
#[cfg(test)]
mod pools_tests;
#[cfg(test)]
mod process_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tree_key_tests;
