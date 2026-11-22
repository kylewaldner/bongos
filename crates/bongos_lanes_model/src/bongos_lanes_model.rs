//! Lane model for bongos: the lane table (`lanes.toml`) parsed with line numbers and validated,
//! the expansion of binding tokens, the run state of lanes, and the picker with the graph view
//! it reads.
//!
//! This crate is plain Rust: no gpui, no store, no file access.

pub mod binding;
pub mod graph;
pub mod picker;
pub mod problem;
pub mod rank;
pub mod stamp;
pub mod state;
pub mod table;

pub use binding::{TokenError, TokenValues, check_tokens, expand_tokens};
pub use bongos_workflow_model::EnvEdit;
pub use graph::{
    EdgeKind, LiveFacts, OpenTodo, PickEdge, PickGraph, PickTask, Shape, TaskState, from_work_tree,
};
pub use picker::{
    AssignedWork, Assignment, DryRun, InstanceUse, LaneHold, OwnerChange, PickBound, PickOptions,
    PickOutcome, Waiting, area_conflict, explain, parallel, pick, table_lanes,
};
pub use problem::Problem;
pub use rank::{Chains, RankKey, RankLevel, critical_path};
pub use stamp::{GraphStamp, STAMP_SCHEMA, StampDiff, StampError, StampFile, StampTask};
pub use state::{
    AdoptOutcome, AdoptRefusal, AdoptRequest, Author, Bound, CountPlan, JobEnd, JobEndInput,
    JobRecord, LaneId, LaneRecord, LaneState, LanesState, OwnerRecord, OwnerState, PinTarget,
    RunSubject, StopCause, TaskId, Transition, WaitReason, adopt, lane_after_retry,
    owner_adoptable_by, owner_after_job_end, owner_after_stop, reconcile_counts, short_run,
    stop_at_job_boundary, stop_lane,
};
pub use table::{
    Binding, Diagnostic, DispatchRule, Exemption, LaneKind, LaneTable, ParseError, Pin, TreeSpec,
    ValidationContext, names_lane_instance, parse, prompt_lane_problems, validate_lane_table,
};

#[cfg(test)]
mod binding_tests;
#[cfg(test)]
mod graph_tests;
#[cfg(test)]
mod picker_tests;
#[cfg(test)]
mod problem_tests;
#[cfg(test)]
mod stamp_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod table_tests;
