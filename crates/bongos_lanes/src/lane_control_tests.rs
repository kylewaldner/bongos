//! Pause, resume, stop and bounds, over the fake host and engine of the dispatcher tests.

use bongos_lanes_model::{
    Author, LaneId, LaneRecord, LaneState, OwnerRecord, OwnerState, PickBound, PickGraph, PickTask,
    TaskId, WaitReason,
};
use bongos_workflow::RunStatus;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{BorrowAppContext, TestAppContext};

use super::{DISPATCHER_RUN, Harness, builds, finish, graph, harness, leaf, start, start_request};
use crate::StartLanes;
use crate::lane_control::{BOUND_REASON, DispatcherPhase, LaneEvent, LaneTarget, RunEnd};
use crate::store::{put_lane, put_owner};
use crate::test_support::SINCE;

fn kim() -> Author {
    Author::Person("kim".to_string())
}

fn lane_id(index: u32) -> LaneId {
    LaneId::new("build", index)
}

fn events(rx: &mut UnboundedReceiver<LaneEvent>) -> Vec<LaneEvent> {
    let mut found = Vec::new();
    while let Ok(event) = rx.try_recv() {
        found.push(event);
    }
    found
}

fn set_lane(harness: &Harness, index: u32, f: impl FnOnce(&mut LaneRecord)) {
    let mut lane = harness.lane("build", index);
    f(&mut lane);
    harness.writer.transact(|tx| put_lane(tx, &lane)).unwrap();
}

fn halted_owner(index: u32) -> OwnerRecord {
    OwnerRecord {
        lane: Some(lane_id(index)),
        state: OwnerState::Halted,
        since: SINCE.to_string(),
        by: Author::System,
        reason: Some("run failed".to_string()),
        prepared_commit: None,
        refreshed: false,
    }
}

fn other(text: &str) -> Option<WaitReason> {
    Some(WaitReason::Other(text.to_string()))
}

// A stop with no lane stops a waiting lane with no job at once, leaves the halted
// and the paused lane as they are, and ends the dispatcher when the running lane stops. The next
// start brings the halted and the paused lane back unchanged.
#[gpui::test]
async fn stop_all_stops_waiting_now_and_ends_when_the_running_lane_stops(cx: &mut TestAppContext) {
    let harness = harness(&builds(4), graph(vec![leaf("a")]), &[]);
    let mut rx = harness.lanes.subscribe();
    let started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);

    set_lane(&harness, 2, |lane| {
        lane.state = LaneState::Halted;
        lane.reason = Some(WaitReason::Failed("run failed".to_string()));
    });
    harness
        .writer
        .transact(|tx| put_owner(tx, &TaskId::new("h"), &halted_owner(2)))
        .unwrap();
    harness.lanes.pause(&lane_id(3), &kim()).unwrap();
    set_lane(&harness, 4, |lane| {
        lane.state = LaneState::Waiting;
        lane.reason = Some(WaitReason::Environment("drive missing".to_string()));
    });
    cx.run_until_parked();

    let status = harness.lanes.status().unwrap();
    assert_eq!(status.phase, Some(DispatcherPhase::Running));
    let row = |index: u32| {
        status
            .lanes
            .iter()
            .find(|l| l.id == lane_id(index))
            .unwrap()
    };
    assert_eq!(row(2).state, LaneState::Halted);
    assert_eq!(
        row(2).reason,
        Some(WaitReason::Failed("run failed".to_string()))
    );
    assert_eq!(row(2).task, Some(TaskId::new("h")));
    assert_eq!(row(3).state, LaneState::Paused);
    assert_eq!(row(3).reason, other("paused by kim"));

    cx.update(|cx| harness.lanes.stop(LaneTarget::All, kim(), cx))
        .unwrap();
    cx.run_until_parked();

    assert_eq!(harness.lane("build", 4).state, LaneState::Stopped);
    let running = harness.lane("build", 1);
    assert_eq!(running.state, LaneState::Running);
    assert!(running.stop_requested.is_some());
    let halted = harness.lane("build", 2);
    assert_eq!(halted.state, LaneState::Halted);
    assert_eq!(
        halted.reason,
        Some(WaitReason::Failed("run failed".to_string()))
    );
    assert_eq!(
        harness.state().owners[&TaskId::new("h")].state,
        OwnerState::Halted
    );
    assert_eq!(harness.lane("build", 3).state, LaneState::Paused);
    assert!(events(&mut rx).is_empty());
    assert!(harness.engine.ends.borrow().is_empty());
    assert_eq!(
        harness.lanes.status().unwrap().phase,
        Some(DispatcherPhase::Pausing)
    );

    harness.update_graph(|_| {});
    harness.set_run("a", RunStatus::Succeeded, cx);

    assert_eq!(harness.lane("build", 1).state, LaneState::Stopped);
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].state,
        OwnerState::Stopped
    );
    assert_eq!(
        events(&mut rx),
        [LaneEvent::DispatcherEnded {
            run_id: DISPATCHER_RUN.to_string(),
            end: RunEnd::Cancelled,
            note: "stopped by kim".to_string(),
        }]
    );
    assert_eq!(
        *harness.engine.ends.borrow(),
        [(
            DISPATCHER_RUN.to_string(),
            RunEnd::Cancelled,
            "stopped by kim".to_string()
        )]
    );
    assert_eq!(
        harness.lanes.status().unwrap().phase,
        Some(DispatcherPhase::Ended(RunEnd::Cancelled))
    );
    drop(started);

    let _again = start(&harness, start_request(), cx).await;
    let halted = harness.lane("build", 2);
    assert_eq!(halted.state, LaneState::Halted);
    assert_eq!(
        halted.reason,
        Some(WaitReason::Failed("run failed".to_string()))
    );
    assert_eq!(halted.stop_requested, None);
    assert_eq!(
        harness.state().owners[&TaskId::new("h")].state,
        OwnerState::Halted
    );
    let paused = harness.lane("build", 3);
    assert_eq!(paused.state, LaneState::Paused);
    assert_eq!(paused.reason, other("paused by kim"));
    assert_eq!(paused.stop_requested, None);
    // The stopped lane is idle again and takes its own stopped task first.
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.engine.job_tasks(), ["a", "a"]);
    assert_eq!(harness.lane("build", 4).state, LaneState::Idle);
}

// A stop never approves a gate: a lane whose run waits on the person keeps waiting, then stops
// once the person's answer ends the run.
#[gpui::test]
async fn a_stop_leaves_a_lane_at_a_gate_until_the_person_answers(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let mut rx = harness.lanes.subscribe();
    let _started = start(&harness, start_request(), cx).await;
    harness.set_run("a", RunStatus::WaitingOnYou, cx);

    cx.update(|cx| harness.lanes.stop(LaneTarget::All, kim(), cx))
        .unwrap();
    cx.run_until_parked();

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Running);
    assert!(lane.job.is_some());
    assert!(events(&mut rx).is_empty());
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    harness.set_run("a", RunStatus::Succeeded, cx);
    assert_eq!(harness.lane("build", 1).state, LaneState::Stopped);
    assert_eq!(events(&mut rx).len(), 1);
}

// A stop on one lane stops that lane only.
#[gpui::test]
async fn stop_on_one_lane_stops_only_that_lane(cx: &mut TestAppContext) {
    let harness = harness(&builds(2), graph(vec![leaf("a"), leaf("b")]), &[]);
    let mut rx = harness.lanes.subscribe();
    let _started = start(&harness, start_request(), cx).await;

    cx.update(|cx| harness.lanes.stop(LaneTarget::Lane(lane_id(1)), kim(), cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(harness.lane("build", 2).stop_requested, None);

    harness.set_run("a", RunStatus::Succeeded, cx);

    assert_eq!(harness.lane("build", 1).state, LaneState::Stopped);
    assert_eq!(harness.lane("build", 2).state, LaneState::Running);
    assert!(events(&mut rx).is_empty());
}

// A pause is immediate on an idle lane and waits for the job boundary of a running one; resume
// clears a pause and a stop.
#[gpui::test]
async fn pause_and_resume(cx: &mut TestAppContext) {
    let harness = harness(&builds(2), graph(vec![leaf("a")]), &[]);
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.lane("build", 2).state, LaneState::Idle);

    harness.lanes.pause(&lane_id(2), &kim()).unwrap();
    let idle = harness.lane("build", 2);
    assert_eq!(idle.state, LaneState::Paused);
    assert_eq!(idle.reason, other("paused by kim"));

    harness.lanes.pause(&lane_id(1), &kim()).unwrap();
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert!(harness.lane("build", 1).pause_requested);

    harness.set_run("a", RunStatus::Succeeded, cx);
    let paused = harness.lane("build", 1);
    assert_eq!(paused.state, LaneState::Paused);
    assert_eq!(paused.reason, other("paused by kim"));
    assert_eq!(paused.job, None);
    assert!(!paused.pause_requested);
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].state,
        OwnerState::Active
    );

    harness.lanes.resume(&lane_id(1)).unwrap();
    cx.run_until_parked();
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.engine.job_tasks(), ["a", "a"]);

    cx.update(|cx| harness.lanes.stop(LaneTarget::Lane(lane_id(2)), kim(), cx))
        .unwrap();
    assert_eq!(harness.lane("build", 2).state, LaneState::Paused);
    assert!(harness.lane("build", 2).stop_requested.is_some());
    assert_eq!(
        harness.lanes.resume(&lane_id(2)).unwrap(),
        "build-2 resumed"
    );
    let resumed = harness.lane("build", 2);
    assert_eq!(resumed.stop_requested, None);
    assert_ne!(resumed.state, LaneState::Paused);

    cx.update(|cx| harness.lanes.stop(LaneTarget::Lane(lane_id(2)), kim(), cx))
        .unwrap();
    assert_eq!(harness.lane("build", 2).state, LaneState::Stopped);
    harness.lanes.resume(&lane_id(2)).unwrap();
    assert_ne!(harness.lane("build", 2).state, LaneState::Stopped);

    assert_eq!(
        harness.lanes.resume(&lane_id(1)).unwrap(),
        "build-1 is already running"
    );
}

fn bound_request(bound: Option<PickBound>, max_jobs: Option<u32>) -> StartLanes {
    StartLanes {
        bound,
        max_jobs,
        ..start_request()
    }
}

fn assert_bound_end(harness: &Harness, rx: &mut UnboundedReceiver<LaneEvent>) {
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Stopped);
    assert_eq!(lane.reason, other(BOUND_REASON));
    let found = events(rx);
    assert_eq!(found.len(), 1);
    assert!(matches!(
        &found[0],
        LaneEvent::DispatcherEnded {
            end: RunEnd::Succeeded,
            ..
        }
    ));
    assert_eq!(harness.engine.ends.borrow()[0].1, RunEnd::Succeeded);
    assert_eq!(
        harness.lanes.status().unwrap().phase,
        Some(DispatcherPhase::Ended(RunEnd::Succeeded))
    );
}

// A lane takes no new top-level task after the `until` task, then stops for the bound.
#[gpui::test]
async fn the_until_bound_stops_the_lane_cleanly(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a"), leaf("b")]), &[]);
    let mut rx = harness.lanes.subscribe();
    let request = bound_request(Some(PickBound::Until(TaskId::new("a"))), None);
    let _started = start(&harness, request, cx).await;
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);

    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert_bound_end(&harness, &mut rx);
}

// A lane stops after `max_jobs` jobs.
#[gpui::test]
async fn the_max_jobs_bound_stops_the_lane_cleanly(cx: &mut TestAppContext) {
    let harness = harness(
        &builds(1),
        graph(vec![leaf("a"), leaf("b"), leaf("c")]),
        &[],
    );
    let mut rx = harness.lanes.subscribe();
    let _started = start(&harness, bound_request(None, Some(2)), cx).await;

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    harness.update_graph(|tasks| finish(tasks, "b"));
    harness.set_run("b", RunStatus::Succeeded, cx);

    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
    assert_eq!(harness.lane("build", 1).jobs_done, 2);
    assert_bound_end(&harness, &mut rx);
}

// A lane takes only leaves under the `within` group, then stops for the bound.
#[gpui::test]
async fn the_within_bound_stops_the_lane_cleanly(cx: &mut TestAppContext) {
    let tasks = vec![leaf("a"), PickTask::group("G", &["g"]), leaf("g")];
    let top = vec![TaskId::new("a"), TaskId::new("G")];
    let harness = harness(&builds(1), PickGraph::new(top, tasks), &[]);
    let mut rx = harness.lanes.subscribe();
    let request = bound_request(Some(PickBound::Within(TaskId::new("G"))), None);
    let _started = start(&harness, request, cx).await;
    assert_eq!(harness.engine.job_tasks(), ["g"]);

    harness.update_graph(|tasks| finish(tasks, "g"));
    harness.set_run("g", RunStatus::Succeeded, cx);

    assert_eq!(harness.engine.job_tasks(), ["g"]);
    assert_bound_end(&harness, &mut rx);
}

/// Registers the lanes settings with `lanes.exit_when_idle` set to `on`.
fn use_exit_when_idle(on: bool, cx: &mut TestAppContext) {
    cx.update(|cx| {
        use settings::Settings as _;
        let store = settings::SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        crate::LanesSettings::register(cx);
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.lanes.get_or_insert_default().exit_when_idle = Some(on);
            });
        });
    });
}

/// The timed pass of the dispatcher (the test host looks every 60 seconds), on the test clock.
fn timed_pass(cx: &mut TestAppContext) {
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(61));
    cx.run_until_parked();
}

// With `lanes.exit_when_idle` true the dispatcher's run ends, succeeded, once every lane is idle
// and nothing is left to take, so the engine host is no longer kept busy. A task that only waits
// (here a held one) is something left, through the timed passes too.
#[gpui::test]
async fn exit_when_idle_ends_the_run_once_nothing_is_left_to_take(cx: &mut TestAppContext) {
    use_exit_when_idle(true, cx);
    let mut held = leaf("b");
    held.held = Some("restore in progress".to_string());
    let harness = harness(&builds(1), graph(vec![leaf("a"), held]), &[]);
    let started = start(&harness, start_request(), cx).await;
    let dispatcher = started.dispatcher.unwrap();
    let mut rx = dispatcher.subscribe();
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);
    assert_eq!(harness.lane("build", 1).state, LaneState::Idle);
    timed_pass(cx);
    assert!(!dispatcher.has_ended());
    assert!(harness.engine.ends.borrow().is_empty());
    assert!(events(&mut rx).is_empty());

    harness.update_graph(|tasks| {
        for task in tasks.iter_mut() {
            task.held = None;
        }
        finish(tasks, "b");
    });
    timed_pass(cx);
    assert!(dispatcher.has_ended());
    let note = crate::lane_control::NOTHING_LEFT.to_string();
    assert_eq!(
        *harness.engine.ends.borrow(),
        [(DISPATCHER_RUN.to_string(), RunEnd::Succeeded, note.clone())]
    );
    assert_eq!(
        events(&mut rx),
        [LaneEvent::DispatcherEnded {
            run_id: DISPATCHER_RUN.to_string(),
            end: RunEnd::Succeeded,
            note,
        }]
    );
    assert_eq!(
        dispatcher.status().unwrap().phase,
        Some(DispatcherPhase::Ended(RunEnd::Succeeded))
    );
    assert_eq!(harness.lane("build", 1).state, LaneState::Idle);
}

// With `lanes.exit_when_idle` false (the default) an idle dispatcher with nothing left keeps
// running.
#[gpui::test]
async fn without_exit_when_idle_an_idle_dispatcher_keeps_running(cx: &mut TestAppContext) {
    use_exit_when_idle(false, cx);
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let started = start(&harness, start_request(), cx).await;
    let dispatcher = started.dispatcher.unwrap();

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);
    timed_pass(cx);

    assert_eq!(harness.lane("build", 1).state, LaneState::Idle);
    assert!(!dispatcher.has_ended());
    assert!(harness.engine.ends.borrow().is_empty());
}
