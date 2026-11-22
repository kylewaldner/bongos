//! The job hooks around a lane job's start, over the fake host and engine of the dispatcher
//! tests, with gated fake hooks.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bongos_lanes_model::{Author, LaneState, OwnerState, TaskId, WaitReason};
use bongos_workflow::Refusal;
use bongos_workflow::RunStatus;
use bongos_workflow::lifecycle::RefusalKind;
use bongos_workflow_model::record::{EndState, Lifecycle};
use gpui::TestAppContext;

use super::{Harness, builds, finish, graph, harness, leaf, start, start_request};
use crate::hooks::{BEFORE_JOB, BeforeJob, JobEnd, JobHooks};
use crate::lane_control::LaneTarget;
use crate::store::put_lane;
use crate::test_support::{GatedHook, HookCall};

fn hook_wait(message: &str) -> WaitReason {
    WaitReason::Hook {
        point: BEFORE_JOB.to_string(),
        message: message.to_string(),
    }
}

fn register(cx: &mut TestAppContext, hook: &Arc<GatedHook>) {
    let hook = hook.clone();
    cx.update(|cx| JobHooks::register(hook, cx));
}

/// A hook whose `before_job` waits for the test.
fn gated_before() -> Arc<GatedHook> {
    let hook = Arc::new(GatedHook::default());
    hook.gate_before.store(true, Ordering::SeqCst);
    hook
}

/// A hook whose `after_job` waits for the test.
fn gated_after() -> Arc<GatedHook> {
    let hook = Arc::new(GatedHook::default());
    hook.gate_after.store(true, Ordering::SeqCst);
    hook
}

fn run_of_lane(harness: &Harness) -> String {
    harness
        .lane("build", 1)
        .job
        .expect("the lane holds a job")
        .run_id
}

fn stop_all(started: &crate::LanesStarted, cx: &mut TestAppContext) {
    let dispatcher = started.dispatcher.as_ref().unwrap();
    cx.update(|cx| {
        dispatcher
            .stop(LaneTarget::All, Author::Person("kim".to_string()), cx)
            .unwrap()
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn set_wait_shows_the_lane_waiting_while_before_job_is_pending(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let hook = gated_before();
    *hook.wait.lock().unwrap() = Some(hook_wait("waiting for a slot"));
    register(cx, &hook);

    let _started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.reason, Some(hook_wait("waiting for a slot")));
    let run = run_of_lane(&harness);
    assert!(harness.engine.job_tasks().is_empty());
    assert_eq!(hook.calls(), [HookCall::Before(run.clone())]);

    hook.decide(BeforeJob::Go);
    cx.run_until_parked();

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Running);
    assert_eq!(lane.reason, None);
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert_eq!(hook.calls(), [HookCall::Before(run)]);
}

#[gpui::test]
async fn a_refusal_leaves_the_job_unstarted_and_the_lane_waiting(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let hook = Arc::new(GatedHook::default());
    *hook.answer.lock().unwrap() = Some(BeforeJob::Refuse {
        reason: hook_wait("no slot"),
    });
    register(cx, &hook);

    let started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.reason, Some(hook_wait("no slot")));
    assert_eq!(lane.job, None);
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.state, OwnerState::Active);
    assert_eq!(owner.lane, Some(lane.id()));
    assert!(harness.engine.job_tasks().is_empty());
    let calls = hook.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(matches!(calls[0], HookCall::Before(_)), "{calls:?}");

    // The next pass evaluates the lane again: the hook is asked again, and never told the end.
    cx.update(|cx| started.dispatcher.as_ref().unwrap().pick_now(cx));
    cx.run_until_parked();
    let calls = hook.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls.iter().all(|call| matches!(call, HookCall::Before(_))),
        "{calls:?}"
    );
}

#[gpui::test]
async fn a_later_refusal_tells_the_hooks_that_said_go(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let first = Arc::new(GatedHook::default());
    let second = Arc::new(GatedHook::default());
    *second.answer.lock().unwrap() = Some(BeforeJob::Refuse {
        reason: hook_wait("no slot"),
    });
    register(cx, &first);
    register(cx, &second);

    let _started = start(&harness, start_request(), cx).await;

    let HookCall::Before(run) = first.calls()[0].clone() else {
        panic!("{:?}", first.calls());
    };
    assert_eq!(
        first.calls(),
        [
            HookCall::Before(run.clone()),
            HookCall::After(run.clone(), JobEnd::StoppedBeforeStart)
        ]
    );
    assert_eq!(second.calls(), [HookCall::Before(run)]);
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.reason, Some(hook_wait("no slot")));
    assert_eq!(lane.job, None);
    assert!(harness.engine.job_tasks().is_empty());
}

#[gpui::test]
async fn after_job_resolves_before_the_lane_is_evaluated_again(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a"), leaf("b")]), &[]);
    let hook = gated_after();
    register(cx, &hook);

    let started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    let run = run_of_lane(&harness);

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);

    assert_eq!(
        hook.calls(),
        [
            HookCall::Before(run.clone()),
            HookCall::After(
                run,
                JobEnd::Lifecycle(Lifecycle::Ended(EndState::Succeeded))
            )
        ]
    );
    assert_eq!(harness.lane("build", 1).state, LaneState::Idle);
    cx.update(|cx| started.dispatcher.as_ref().unwrap().pick_now(cx));
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    hook.release_after();
    cx.run_until_parked();

    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
    assert_eq!(hook.calls().len(), 3);
}

#[gpui::test]
async fn after_job_hears_a_refused_start(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    *harness.engine.refuse.borrow_mut() = Some(Refusal::new(
        RefusalKind::LowDiskSpace,
        "Only 1 GB is free.",
        "Free some space.",
    ));
    let hook = gated_after();
    register(cx, &hook);

    let _started = start(&harness, start_request(), cx).await;

    let run = run_of_lane(&harness);
    assert_eq!(
        hook.calls(),
        [
            HookCall::Before(run.clone()),
            HookCall::After(
                run,
                JobEnd::Refused {
                    reason: "Only 1 GB is free. Free some space.".to_string()
                }
            )
        ]
    );
    // The job is cleared only once `after_job` resolved.
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);

    hook.release_after();
    cx.run_until_parked();

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.job, None);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Environment(
            "Only 1 GB is free. Free some space.".to_string()
        ))
    );
}

#[gpui::test]
async fn a_stop_drops_a_pending_before_job(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let hook = gated_before();
    hook.gate_after.store(true, Ordering::SeqCst);
    *hook.wait.lock().unwrap() = Some(hook_wait("waiting for a slot"));
    register(cx, &hook);
    let started = start(&harness, start_request(), cx).await;
    let run = run_of_lane(&harness);
    assert_eq!(hook.dropped(), 0);

    stop_all(&started, cx);

    assert_eq!(hook.dropped(), 1);
    assert_eq!(
        hook.calls(),
        [
            HookCall::Before(run.clone()),
            HookCall::After(run.clone(), JobEnd::StoppedBeforeStart)
        ]
    );
    // The lane keeps the job, and the dispatcher runs, until `after_job` resolved.
    assert_eq!(run_of_lane(&harness), run);
    assert!(harness.engine.ends.borrow().is_empty());

    hook.release_after();
    cx.run_until_parked();

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Stopped);
    assert_eq!(lane.job, None);
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].state,
        OwnerState::Stopped
    );
    assert!(harness.engine.job_tasks().is_empty());
    assert_eq!(harness.engine.ends.borrow().len(), 1);
    assert_eq!(hook.calls().len(), 2);
}

#[gpui::test]
async fn a_stop_after_go_drops_the_job_before_its_run(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let first = Arc::new(GatedHook::default());
    let second = gated_before();
    register(cx, &first);
    register(cx, &second);
    let started = start(&harness, start_request(), cx).await;
    let run = run_of_lane(&harness);
    assert_eq!(first.calls(), [HookCall::Before(run.clone())]);

    stop_all(&started, cx);

    let told = [
        HookCall::Before(run.clone()),
        HookCall::After(run, JobEnd::StoppedBeforeStart),
    ];
    assert_eq!(first.calls(), told);
    assert_eq!(second.calls(), told);
    assert_eq!(first.dropped(), 0);
    assert_eq!(second.dropped(), 1);
    assert_eq!(harness.lane("build", 1).state, LaneState::Stopped);
    assert!(harness.engine.job_tasks().is_empty());
}

// The answer `Go` and the stop arrive together; whichever the dispatcher sees first, the run is
// not started and the hook is told once.
#[gpui::test]
async fn a_stop_with_the_go_drops_the_job_before_its_run(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let hook = gated_before();
    register(cx, &hook);
    let started = start(&harness, start_request(), cx).await;
    let run = run_of_lane(&harness);

    hook.decide(BeforeJob::Go);
    stop_all(&started, cx);

    assert_eq!(
        hook.calls(),
        [
            HookCall::Before(run.clone()),
            HookCall::After(run, JobEnd::StoppedBeforeStart)
        ]
    );
    assert_eq!(harness.lane("build", 1).state, LaneState::Stopped);
    assert!(harness.engine.job_tasks().is_empty());
}

// A lane that loses its job while `before_job` is pending (another writer cleared it) drops it.
#[gpui::test]
async fn a_lost_claim_drops_a_pending_before_job(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let hook = gated_before();
    register(cx, &hook);
    let _started = start(&harness, start_request(), cx).await;
    let run = run_of_lane(&harness);

    let mut lane = harness.lane("build", 1);
    harness
        .writer
        .transact(|tx| {
            lane.state = LaneState::Idle;
            lane.job = None;
            put_lane(tx, &lane)
        })
        .unwrap();
    cx.run_until_parked();

    assert_eq!(hook.dropped(), 1);
    let calls = hook.calls();
    assert_eq!(
        calls[..2],
        [
            HookCall::Before(run.clone()),
            HookCall::After(run.clone(), JobEnd::StoppedBeforeStart)
        ]
    );
    // Once told, the lane is evaluated again and takes the task with a new job.
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(matches!(&calls[2], HookCall::Before(next) if *next != run));
    assert!(harness.engine.job_tasks().is_empty());
}

#[gpui::test]
async fn after_job_is_called_once_for_each_go_at_the_run_end(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let first = Arc::new(GatedHook::default());
    let second = Arc::new(GatedHook::default());
    register(cx, &first);
    register(cx, &second);
    let _started = start(&harness, start_request(), cx).await;
    let run = run_of_lane(&harness);

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);
    // A repeated end changes nothing.
    cx.update(|cx| harness.engine.set(&run, RunStatus::Succeeded, cx));
    cx.run_until_parked();

    let told = [
        HookCall::Before(run.clone()),
        HookCall::After(
            run,
            JobEnd::Lifecycle(Lifecycle::Ended(EndState::Succeeded)),
        ),
    ];
    assert_eq!(first.calls(), told);
    assert_eq!(second.calls(), told);
    assert_eq!(harness.lane("build", 1).state, LaneState::Idle);
}

// A halted run keeps its lane's job; its cancel ends the job, and only then is `after_job` called.
#[gpui::test]
async fn after_job_waits_for_the_end_of_a_halted_job(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let hook = Arc::new(GatedHook::default());
    register(cx, &hook);
    let _started = start(&harness, start_request(), cx).await;
    let run = run_of_lane(&harness);

    harness.set_run("a", RunStatus::Failed, cx);
    assert_eq!(harness.lane("build", 1).state, LaneState::Halted);
    assert_eq!(hook.calls(), [HookCall::Before(run.clone())]);

    harness.set_run("a", RunStatus::Cancelled, cx);
    assert_eq!(
        hook.calls(),
        [
            HookCall::Before(run.clone()),
            HookCall::After(run, JobEnd::Lifecycle(Lifecycle::Cancelled))
        ]
    );
}
