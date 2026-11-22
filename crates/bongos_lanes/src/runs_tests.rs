//! Tests of the lanes state as the project repository reads it: the live source, the activity
//! source and the owned scope of a lane run.

use std::collections::BTreeMap;
use std::sync::Arc;

use bongos_lanes_model::{LaneId, OwnerState, TaskId};
use bongos_project_repo::{ActivitySource, LiveSource};
use bongos_project_repo_model::checks::{CheckOutcome, LiveFacts, StartedInput, started_unchanged};
use bongos_project_repo_model::{Author, ChangeMeta, summarize};
use tempfile::TempDir;

use crate::runs::{LaneActivity, LaneLiveSource, LaneRuns, OwnedScope};
use crate::store::write_state;
use crate::test_support::{lanes_state, tree_with, writer};

const PROJECT: &str = "project-1";

fn runs() -> LaneRuns {
    let runs = LaneRuns::default();
    runs.set(PROJECT, lanes_state());
    runs
}

#[test]
fn lanes_a_lane_owned_task_counts_as_started() {
    let source = LaneLiveSource { runs: runs() };
    assert!(source.started("feature"), "the task build-1 owns");
    assert!(source.started("leaf"), "the task build-1 runs a job on");
    assert!(!source.started("shipped"), "a done owner holds nothing");
    assert!(!source.started("other"));

    let mut live = LiveFacts::from_states(BTreeMap::new());
    live.register(Arc::new(source));
    assert!(live.started("feature"));

    // Check 6 keeps the shape of the owned task.
    let master = tree_with(&[("tasks/feature", ""), ("tasks/other", "")]);
    let new = tree_with(&[
        ("tasks/feature", "size = 3\n"),
        ("tasks/other", "size = 3\n"),
    ]);
    let summary = summarize(&master, &new);
    let meta = ChangeMeta::new("resize", Author::agent("agent"));
    let outcome = started_unchanged(&StartedInput {
        master: &master,
        new: &new,
        summary: &summary,
        live: &live,
        change_id: "c1",
        meta: &meta,
        staged_moves: &[],
    });
    assert_eq!(
        outcome.message(),
        Some("started task feature: its size may not change")
    );
    assert_ne!(outcome, CheckOutcome::Pass);
}

#[test]
fn lanes_an_owner_counts_as_started_while_active_stopped_halted_or_ahead() {
    for (state, started) in [
        (OwnerState::Active, true),
        (OwnerState::Stopped, true),
        (OwnerState::Halted, true),
        (OwnerState::Ahead, true),
        (OwnerState::Prepared, false),
        (OwnerState::Done, false),
    ] {
        let mut lanes = lanes_state();
        lanes.owners.get_mut(&TaskId::new("feature")).unwrap().state = state;
        // No job on the task, so only the owner record can make it started.
        for lane in lanes.lanes.values_mut() {
            lane.job = None;
        }
        let runs = LaneRuns::default();
        runs.set(PROJECT, lanes);
        let source = LaneLiveSource { runs };
        assert_eq!(source.started("feature"), started, "{state}");
    }
}

#[test]
fn lanes_a_task_is_no_longer_started_once_its_lanes_state_is_forgotten() {
    let runs = runs();
    let source = LaneLiveSource { runs: runs.clone() };
    runs.forget(PROJECT);
    assert!(!source.started("feature"));
}

#[test]
fn lanes_runs_load_from_the_state_store() {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    writer
        .transact(|tx| write_state(tx, &lanes_state()))
        .unwrap();
    let runs = LaneRuns::default();
    writer.read(|read| runs.load(PROJECT, read)).unwrap();
    assert_eq!(runs.state(PROJECT), lanes_state());
}

#[test]
fn lanes_the_owned_scope_of_a_lane_run_is_its_lanes_task() {
    let runs = runs();
    assert_eq!(
        runs.owned_scope_in(PROJECT, "run-lane"),
        Some(OwnedScope {
            lane: LaneId::new("build", 1),
            task: TaskId::new("feature"),
        })
    );
    assert_eq!(runs.owned_scope_in(PROJECT, "run-by-hand"), None);
    assert_eq!(runs.owned_scope_in("project-2", "run-lane"), None);
    assert_eq!(runs.owned_scope("run-lane"), Some(TaskId::new("feature")));
    assert_eq!(runs.owned_scope("run-by-hand"), None);
}

#[test]
fn lanes_activity_lists_running_lanes_and_holds_new_work() {
    let runs = runs();
    let activity = LaneActivity { runs: runs.clone() };
    assert_eq!(
        activity.activity(PROJECT),
        ["lane build-1 running build, run run-lane"]
    );
    assert!(activity.activity("project-2").is_empty());
    assert!(
        activity.stop_at_job_boundary(PROJECT).is_err(),
        "lanes stop in the engine host"
    );

    assert!(!runs.is_held(PROJECT));
    activity.hold_new_work(PROJECT);
    activity.hold_new_work(PROJECT);
    activity.release_new_work(PROJECT);
    assert!(runs.is_held(PROJECT));
    activity.release_new_work(PROJECT);
    assert!(!runs.is_held(PROJECT));
}
