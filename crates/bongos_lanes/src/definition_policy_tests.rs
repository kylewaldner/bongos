//! The definition policy for jobs and for the lane table, the staleness script of a prepared
//! task, ahead completion and the snapshot after every job end.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use bongos_lanes_model::{LaneId, LaneState, OwnerState, PickTask, TaskId, WaitReason};
use bongos_workflow::RunStatus;
use gpui::TestAppContext;

use super::{after, builds, finish, graph, harness_with, history_of, leaf, start, start_request};
use crate::dispatch_job::STALENESS_FAILED;
use crate::dispatcher::{AheadLanding, Dispatcher, JobCommit, JobDefinitionPolicy};
use crate::staleness::Staleness;

/// A policy with one answer per lane, `Use(master)` for the others, and a changeable answer
/// for the lane table (`lane = None`), `Use(master)` until set.
#[derive(Default)]
struct TestPolicy {
    lanes: BTreeMap<LaneId, JobCommit>,
    table: RefCell<Option<JobCommit>>,
}

impl JobDefinitionPolicy for TestPolicy {
    fn commit_for_job(&self, lane: Option<&LaneId>, master: &str) -> JobCommit {
        let answer = match lane {
            Some(lane) => self.lanes.get(lane).cloned(),
            None => self.table.borrow().clone(),
        };
        answer.unwrap_or_else(|| JobCommit::Use(master.to_string()))
    }
}

fn build(index: u32) -> LaneId {
    LaneId::new("build", index)
}

/// A build lane whose step rule has a staleness script, and a design lane that prepares ahead.
const AHEAD_TABLE: &str = r#"
[[kind]]
name = "build"
count = 1
rules = ["step"]

[[kind]]
name = "design"
count = 1
ahead = { cap = 1 }
rules = ["prepare"]

[rules.step]
work_kind = "step"
workflow = "implement"
staleness = "lanes/scripts/stale"
refresh = "refresh"

[rules.prepare]
work_kind = "design"
workflow = "prepare"
"#;

/// Feature F: a design leaf, then a step leaf after it.
fn feature() -> Vec<PickTask> {
    let mut design = PickTask::leaf("F.d");
    design.work_kind = Some("design".to_string());
    let mut step = after(PickTask::leaf("F.s"), "F.d");
    step.work_kind = Some("step".to_string());
    vec![PickTask::group("F", &["F.d", "F.s"]), design, step]
}

/// Runs the design lane's preparation of F to success, so F is prepared, and the build lane
/// takes F.s. Returns the harness after that pick, and the dispatcher to keep.
async fn prepared_then_picked(
    staleness: Result<Staleness, String>,
    cx: &mut TestAppContext,
) -> (super::Harness, Dispatcher) {
    let tasks = feature();
    let harness = harness_with(
        AHEAD_TABLE,
        bongos_lanes_model::PickGraph::new(vec![TaskId::new("F")], tasks),
        &[],
        Rc::new(TestPolicy::default()),
    );
    *harness.host.staleness.borrow_mut() = staleness;
    let started = start(&harness, start_request(), cx).await;
    let dispatcher = started.dispatcher.unwrap();

    // The ahead lane prepares F; the build lane waits on F.d.
    assert_eq!(harness.engine.job_tasks(), ["F.d"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Idle);
    let owner = harness.state().owners[&TaskId::new("F")].clone();
    assert_eq!(owner.state, OwnerState::Ahead);

    // The preparation lands and succeeds: F is prepared at the verified commit, with no lane.
    harness.update_graph(|tasks| finish(tasks, "F.d"));
    harness.set_run("F.d", RunStatus::Succeeded, cx);
    (harness, dispatcher)
}

// A stale prepared task runs the refresh first, on the same leaf.
#[gpui::test]
async fn a_stale_prepared_task_runs_the_refresh_first(cx: &mut TestAppContext) {
    let (harness, _dispatcher) = prepared_then_picked(Ok(Staleness::Stale), cx).await;

    let checks = harness.host.staleness_checks.borrow().clone();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0].lane, build(1));
    assert_eq!(checks[0].task, TaskId::new("F"));
    assert_eq!(checks[0].prepared_commit.as_deref(), Some("c1"));
    assert_eq!(checks[0].script, "lanes/scripts/stale");

    let jobs = harness.engine.jobs.borrow();
    assert_eq!(jobs.len(), 2);
    let (request, _) = &jobs[1];
    assert_eq!(request.task_id(), Some("F.s"));
    assert_eq!(request.workflow.as_deref(), Some("refresh"));
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Running);
    assert_eq!(lane.job.as_ref().unwrap().workflow, "refresh");
    let owner = harness.state().owners[&TaskId::new("F")].clone();
    assert_eq!(owner.state, OwnerState::Active);
    assert_eq!(owner.lane, Some(build(1)));
    assert!(owner.refreshed);
    assert!(
        history_of(&harness, "F")
            .iter()
            .any(|event| event == "staleness script says stale; build-1 runs refresh first"),
        "{:?}",
        history_of(&harness, "F")
    );
}

// A fresh prepared task runs its own workflow, and is not checked again.
#[gpui::test]
async fn a_fresh_prepared_task_runs_its_workflow(cx: &mut TestAppContext) {
    let (harness, _dispatcher) = prepared_then_picked(Ok(Staleness::Fresh), cx).await;

    assert_eq!(harness.host.staleness_checks.borrow().len(), 1);
    let jobs = harness.engine.jobs.borrow();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[1].0.workflow.as_deref(), Some("implement"));
    assert!(harness.state().owners[&TaskId::new("F")].refreshed);
}

// A staleness script that answers neither halts the lane with its text, and starts no run.
#[gpui::test]
async fn a_failed_staleness_script_halts_the_lane(cx: &mut TestAppContext) {
    let (harness, _dispatcher) = prepared_then_picked(Err("exit code 3".to_string()), cx).await;

    assert_eq!(harness.engine.job_tasks(), ["F.d"]);
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Halted);
    assert_eq!(lane.job, None);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Failed(format!("{STALENESS_FAILED}exit code 3")))
    );
}

// Ahead completion: a preparation's success leaves its task prepared at the verified commit.
#[gpui::test]
async fn an_ahead_lane_leaves_its_task_prepared(cx: &mut TestAppContext) {
    let harness = harness_with(
        AHEAD_TABLE,
        bongos_lanes_model::PickGraph::new(vec![TaskId::new("F")], feature()),
        &[],
        Rc::new(TestPolicy::default()),
    );
    let started = start(&harness, start_request(), cx).await;
    let _dispatcher = started.dispatcher.unwrap();
    assert_eq!(harness.engine.job_tasks(), ["F.d"]);

    harness.set_run("F.d", RunStatus::Succeeded, cx);

    let owner = harness.state().owners[&TaskId::new("F")].clone();
    assert_eq!(owner.state, OwnerState::Prepared);
    assert_eq!(owner.lane, None);
    assert_eq!(owner.prepared_commit.as_deref(), Some("c1"));
    assert!(!owner.refreshed);
    let design = harness.lane("design", 1);
    assert_eq!(design.state, LaneState::Idle);
    assert_eq!(design.jobs_done, 1);
}

/// The design lane's preparation of F has started, and its landings resolve only when the test
/// says.
async fn preparing_with_gated_landings(cx: &mut TestAppContext) -> (super::Harness, Dispatcher) {
    let harness = harness_with(
        AHEAD_TABLE,
        bongos_lanes_model::PickGraph::new(vec![TaskId::new("F")], feature()),
        &[],
        Rc::new(TestPolicy::default()),
    );
    harness.host.landing_gated.set(true);
    let started = start(&harness, start_request(), cx).await;
    let dispatcher = started.dispatcher.unwrap();
    assert_eq!(harness.engine.job_tasks(), ["F.d"]);
    (harness, dispatcher)
}

// D7: a preparation whose landings are still resolving leaves its owner `Ahead` and its lane
// holding the job; once they land, the task is prepared at the landed commit.
#[gpui::test]
async fn an_ahead_lane_waits_for_its_landings(cx: &mut TestAppContext) {
    let (harness, _dispatcher) = preparing_with_gated_landings(cx).await;
    let run = harness.run_of("F.d");

    harness.set_run("F.d", RunStatus::Succeeded, cx);
    let owner = harness.state().owners[&TaskId::new("F")].clone();
    assert_eq!(owner.state, OwnerState::Ahead);
    let design = harness.lane("design", 1);
    assert_eq!(design.state, LaneState::Running);
    assert_eq!(design.job.as_ref().unwrap().run_id, run);
    assert!(harness.host.snapshots.borrow().is_empty());

    let resolved = harness.host.resolve_landings(AheadLanding::Landed {
        commit: "c7".to_string(),
    });
    assert_eq!(resolved, std::slice::from_ref(&run));
    cx.run_until_parked();

    let owner = harness.state().owners[&TaskId::new("F")].clone();
    assert_eq!(owner.state, OwnerState::Prepared);
    assert_eq!(owner.lane, None);
    assert_eq!(owner.prepared_commit.as_deref(), Some("c7"));
    let design = harness.lane("design", 1);
    assert_eq!(design.state, LaneState::Idle);
    assert_eq!(design.job, None);
    assert_eq!(design.jobs_done, 1);
    assert_eq!(
        *harness.host.snapshots.borrow(),
        [(LaneId::new("design", 1), run, RunStatus::Succeeded)]
    );
}

// D7: a refused landing of a preparation halts the ahead lane and its task with the reason.
#[gpui::test]
async fn a_refused_landing_halts_the_ahead_lane(cx: &mut TestAppContext) {
    let (harness, _dispatcher) = preparing_with_gated_landings(cx).await;
    let run = harness.run_of("F.d");

    harness.set_run("F.d", RunStatus::Succeeded, cx);
    harness
        .host
        .resolve_landings(AheadLanding::Refused("check docs failed".to_string()));
    cx.run_until_parked();

    let text = "landing refused: check docs failed";
    let design = harness.lane("design", 1);
    assert_eq!(design.state, LaneState::Halted);
    assert_eq!(design.job, None);
    assert_eq!(design.reason, Some(WaitReason::Failed(text.to_string())));
    let owner = harness.state().owners[&TaskId::new("F")].clone();
    assert_eq!(owner.state, OwnerState::Halted);
    assert_eq!(owner.lane, Some(LaneId::new("design", 1)));
    assert_eq!(owner.reason.as_deref(), Some(text));
    assert_eq!(owner.prepared_commit, None);
    let short: String = run.chars().take(8).collect();
    assert!(
        history_of(&harness, "F").contains(&format!("run {short} succeeded, but its {text}")),
        "{:?}",
        history_of(&harness, "F")
    );
    assert_eq!(harness.host.snapshots.borrow().len(), 1);
}

// A preparation whose landings were still resolving when its dispatcher went away: the wait goes
// with the dispatcher, and the next dispatcher waits again on its start, so the task is prepared.
#[gpui::test]
async fn a_restarted_dispatcher_waits_again_for_ahead_landings(cx: &mut TestAppContext) {
    let (harness, dispatcher) = preparing_with_gated_landings(cx).await;
    let run = harness.run_of("F.d");
    harness.set_run("F.d", RunStatus::Succeeded, cx);
    assert_eq!(
        harness.host.listening_landings(),
        std::slice::from_ref(&run)
    );

    drop(dispatcher);
    cx.run_until_parked();
    assert!(harness.host.listening_landings().is_empty());
    assert_eq!(
        harness.state().owners[&TaskId::new("F")].state,
        OwnerState::Ahead
    );

    let again = start(&harness, start_request(), cx).await;
    let _dispatcher = again.dispatcher.unwrap();
    assert_eq!(
        harness.host.listening_landings(),
        std::slice::from_ref(&run)
    );
    assert_eq!(harness.engine.job_tasks(), ["F.d"]);
    harness.host.resolve_landings(AheadLanding::Landed {
        commit: "c7".to_string(),
    });
    cx.run_until_parked();

    let owner = harness.state().owners[&TaskId::new("F")].clone();
    assert_eq!(owner.state, OwnerState::Prepared);
    assert_eq!(owner.prepared_commit.as_deref(), Some("c7"));
    let design = harness.lane("design", 1);
    assert_eq!(design.state, LaneState::Idle);
    assert_eq!(design.job, None);
}

// A preparation that succeeded while no dispatcher listened (the app was closed): the next
// dispatcher finds the succeeded run of the running ahead lane and waits for its landings.
#[gpui::test]
async fn an_ahead_run_that_succeeded_unheard_is_waited_on_at_start(cx: &mut TestAppContext) {
    let (harness, dispatcher) = preparing_with_gated_landings(cx).await;
    let run = harness.run_of("F.d");
    drop(dispatcher);
    cx.run_until_parked();
    harness.set_run("F.d", RunStatus::Succeeded, cx);
    assert!(harness.host.listening_landings().is_empty());
    assert_eq!(harness.lane("design", 1).state, LaneState::Running);

    let again = start(&harness, start_request(), cx).await;
    let _dispatcher = again.dispatcher.unwrap();
    assert_eq!(
        harness.host.listening_landings(),
        std::slice::from_ref(&run)
    );
    harness
        .host
        .resolve_landings(AheadLanding::Refused("interrupted".to_string()));
    cx.run_until_parked();

    let design = harness.lane("design", 1);
    assert_eq!(design.state, LaneState::Halted);
    assert_eq!(
        design.reason,
        Some(WaitReason::Failed(
            "landing refused: interrupted".to_string()
        ))
    );
    assert_eq!(
        harness.state().owners[&TaskId::new("F")].state,
        OwnerState::Halted
    );
}

// Each job starts on the commit the policy names for its lane; `Wait` leaves the lane idle.
#[gpui::test]
async fn each_job_starts_on_the_commit_its_lane_is_given(cx: &mut TestAppContext) {
    let policy = TestPolicy {
        lanes: BTreeMap::from([
            (build(1), JobCommit::Use("old".to_string())),
            (build(2), JobCommit::Use("new".to_string())),
            (build(3), JobCommit::Wait("canary first".to_string())),
        ]),
        table: RefCell::new(None),
    };
    let harness = harness_with(
        &builds(3),
        graph(vec![leaf("a"), leaf("b"), leaf("c")]),
        &[],
        Rc::new(policy),
    );
    let started = start(&harness, start_request(), cx).await;
    let _dispatcher = started.dispatcher.unwrap();

    let mut by_lane = BTreeMap::new();
    for (request, _) in harness.engine.jobs.borrow().iter() {
        let lane = LaneId::parse(&request.lane.as_ref().unwrap().id).unwrap();
        let commit = request.definition_commit.clone().map(|commit| commit.0);
        by_lane.insert(lane, commit);
    }
    assert_eq!(
        by_lane,
        BTreeMap::from([
            (build(1), Some("old".to_string())),
            (build(2), Some("new".to_string())),
        ])
    );
    for index in [1, 2] {
        let lane = harness.lane("build", index);
        assert_eq!(lane.state, LaneState::Running);
        let expected = if index == 1 { "old" } else { "new" };
        assert_eq!(
            lane.job.unwrap().definition_commit.as_deref(),
            Some(expected)
        );
    }
    let waiting = harness.lane("build", 3);
    assert_eq!(waiting.state, LaneState::Idle);
    assert_eq!(waiting.job, None);
    assert_eq!(
        waiting.reason,
        Some(WaitReason::Other("canary first".to_string()))
    );
}

// With `Wait` for the lane table, a landed lane-table change is not adopted and `status` says
// why; `StopFirst` counts as `Wait`; `Use(c)` adopts `c`.
#[gpui::test]
async fn the_policy_governs_adopting_a_lane_table_change(cx: &mut TestAppContext) {
    let policy = Rc::new(TestPolicy::default());
    let harness = harness_with(
        &builds(1),
        graph(vec![leaf("a"), leaf("b")]),
        &[],
        policy.clone(),
    );
    harness
        .host
        .tables
        .borrow_mut()
        .insert("c2".to_string(), builds(2));
    let started = start(&harness, start_request(), cx).await;
    let dispatcher = started.dispatcher.unwrap();
    let status = dispatcher.status().unwrap();
    assert_eq!(status.table_commit.as_deref(), Some("c1"));
    assert_eq!(status.table_wait, None);
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    // Wait: the change at c2 (a second build lane) is not adopted.
    *policy.table.borrow_mut() = Some(JobCommit::Wait("canary on the old table".to_string()));
    cx.update(|cx| dispatcher.verified_commit_changed("c2".to_string(), cx));
    cx.run_until_parked();
    let status = dispatcher.status().unwrap();
    assert_eq!(status.table_commit.as_deref(), Some("c1"));
    assert_eq!(
        status.table_wait.as_deref(),
        Some("canary on the old table")
    );
    assert_eq!(dispatcher.table_commit(), "c1");
    assert!(!harness.state().lanes.contains_key(&build(2)));
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    // StopFirst is not valid for the lane table and counts as Wait; no lane is stopped.
    *policy.table.borrow_mut() = Some(JobCommit::StopFirst("roll out".to_string()));
    cx.update(|cx| dispatcher.verified_commit_changed("c2".to_string(), cx));
    cx.run_until_parked();
    let status = dispatcher.status().unwrap();
    assert_eq!(status.table_commit.as_deref(), Some("c1"));
    assert_eq!(status.table_wait.as_deref(), Some("roll out"));
    assert!(!harness.state().lanes.contains_key(&build(2)));
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.lane("build", 1).stop_requested, None);

    // Use(c2) adopts it: the second lane is made and takes b.
    *policy.table.borrow_mut() = Some(JobCommit::Use("c2".to_string()));
    cx.update(|cx| dispatcher.verified_commit_changed("c2".to_string(), cx));
    cx.run_until_parked();
    let status = dispatcher.status().unwrap();
    assert_eq!(status.table_commit.as_deref(), Some("c2"));
    assert_eq!(status.table_wait, None);
    assert_eq!(dispatcher.table_commit(), "c2");
    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
    assert_eq!(harness.lane("build", 2).state, LaneState::Running);
}

// A snapshot follows every job end: success, failure, halt and cancel, and none while running.
#[gpui::test]
async fn a_snapshot_follows_every_job_end(cx: &mut TestAppContext) {
    let harness = harness_with(
        &builds(4),
        graph(vec![leaf("a"), leaf("b"), leaf("c"), leaf("d")]),
        &[],
        Rc::new(TestPolicy::default()),
    );
    let started = start(&harness, start_request(), cx).await;
    let _dispatcher = started.dispatcher.unwrap();
    assert_eq!(harness.engine.job_tasks().len(), 4);
    let runs: BTreeMap<&str, String> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|task| (task, harness.run_of(task)))
        .collect();

    harness.set_run("a", RunStatus::WaitingOnYou, cx);
    assert!(harness.host.snapshots.borrow().is_empty());

    let ends = [
        ("a", RunStatus::Succeeded),
        ("b", RunStatus::Failed),
        ("c", RunStatus::Halted),
        ("d", RunStatus::Cancelled),
    ];
    for (count, (task, status)) in ends.into_iter().enumerate() {
        let run = runs[task].clone();
        cx.update(|cx| harness.engine.set(&run, status, cx));
        cx.run_until_parked();
        let snapshots = harness.host.snapshots.borrow().clone();
        assert_eq!(snapshots.len(), count + 1, "after {task}: {snapshots:?}");
        let (lane, snapshot_run, ended) = snapshots.last().unwrap();
        assert_eq!(snapshot_run, &run);
        assert_eq!(lane, &build(count as u32 + 1));
        assert_eq!(*ended, status);
    }
}
