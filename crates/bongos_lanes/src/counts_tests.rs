//! Lane counts changed by the project profile while the lanes run, and the graph stamp check,
//! over the fake host and engine of the dispatcher tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bongos_lanes_model::{
    LaneState, OwnerState, Problem, StampFile, StopCause, TaskId, WaitReason, parse, stamp,
};
use bongos_workflow::RunStatus;
use gpui::TestAppContext;

use super::{Harness, builds, graph, harness, leaf, start, start_request};
use crate::profile::{LaneProfileSource, LaneProfiles};
use crate::{Dispatcher, LanesStarted};

/// A project profile that says only the lane counts, which a test changes.
#[derive(Default)]
struct CountProfile {
    counts: Mutex<HashMap<String, u32>>,
}

impl LaneProfileSource for CountProfile {
    fn count(&self, _project: &str, kind: &str) -> Option<u32> {
        self.counts.lock().unwrap().get(kind).copied()
    }
}

fn profile(counts: &[(&str, u32)], cx: &mut TestAppContext) -> Arc<CountProfile> {
    let profile = Arc::new(CountProfile::default());
    for (kind, count) in counts {
        profile
            .counts
            .lock()
            .unwrap()
            .insert(kind.to_string(), *count);
    }
    let source = profile.clone();
    cx.update(|cx| LaneProfiles::register(source, cx));
    profile
}

/// The profile's count of `kind` becomes `count`, delivered as the profile's change.
fn set_count(profile: &CountProfile, kind: &str, count: u32, cx: &mut TestAppContext) {
    profile
        .counts
        .lock()
        .unwrap()
        .insert(kind.to_string(), count);
    cx.update(LaneProfiles::changed);
    cx.run_until_parked();
}

fn dispatcher(started: &LanesStarted) -> &Dispatcher {
    started.dispatcher.as_ref().unwrap()
}

fn lane_ids(harness: &Harness) -> Vec<String> {
    harness
        .state()
        .lanes
        .keys()
        .map(ToString::to_string)
        .collect()
}

// A count raised by the profile makes the lane and it works at once; a count lowered stops
// the running lane above it only after its job, never mid-job, with no restart.
#[gpui::test]
async fn a_profile_count_change_applies_at_the_job_boundary_with_no_restart(
    cx: &mut TestAppContext,
) {
    let harness = harness(
        &builds(1),
        graph(vec![leaf("a"), leaf("b"), leaf("c")]),
        &[],
    );
    let profile = profile(&[("build", 1)], cx);
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(lane_ids(&harness), ["build-1"]);
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    set_count(&profile, "build", 2, cx);
    assert_eq!(lane_ids(&harness), ["build-1", "build-2"]);
    assert_eq!(harness.lane("build", 2).state, LaneState::Running);
    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
    assert_eq!(harness.engine.dispatcher_starts.borrow().len(), 1);
    for lane in harness.state().lanes.values() {
        assert_eq!(lane.stop_requested, None, "{}", lane.id());
    }

    let job = harness.lane("build", 2).job;
    set_count(&profile, "build", 1, cx);
    let lowered = harness.lane("build", 2);
    assert_eq!(lowered.state, LaneState::Running);
    assert_eq!(lowered.job, job);
    assert_eq!(lowered.stop_requested, Some(StopCause::Count));
    assert_eq!(harness.lane("build", 1).stop_requested, None);

    harness.set_run("b", RunStatus::Succeeded, cx);
    assert_eq!(lane_ids(&harness), ["build-1"]);
    let owner = &harness.state().owners[&TaskId::new("b")];
    assert_eq!(owner.state, OwnerState::Stopped);
    assert_eq!(owner.reason.as_deref(), Some("lane count lowered"));
    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.engine.dispatcher_starts.borrow().len(), 1);

    // Raised again, the lane comes back and takes its stopped task first.
    set_count(&profile, "build", 2, cx);
    assert_eq!(lane_ids(&harness), ["build-1", "build-2"]);
    assert_eq!(harness.engine.job_tasks(), ["a", "b", "b"]);
}

// A count change with no lane above the new count stops none; a free lane above a lowered count
// goes at once.
#[gpui::test]
async fn a_count_change_stops_only_lanes_above_the_count(cx: &mut TestAppContext) {
    let table = format!("{}[[kind]]\nname = \"docs\"\ncount = 1\n", builds(2));
    let harness = harness(&table, graph(vec![leaf("a")]), &[]);
    let profile = profile(&[], cx);
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.lane("build", 2).state, LaneState::Idle);

    set_count(&profile, "docs", 2, cx);
    assert_eq!(
        lane_ids(&harness),
        ["build-1", "build-2", "docs-1", "docs-2"]
    );
    for lane in harness.state().lanes.values() {
        assert_eq!(lane.stop_requested, None, "{}", lane.id());
        assert_ne!(lane.state, LaneState::Stopped, "{}", lane.id());
    }

    set_count(&profile, "build", 1, cx);
    assert_eq!(lane_ids(&harness), ["build-1", "docs-1", "docs-2"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.lane("build", 1).stop_requested, None);
}

// The overlap check runs again on a count change; a new lane whose locations overlap
// another's waits naming the pair, and takes no job, until a change clears it.
#[gpui::test]
async fn a_new_lane_whose_locations_overlap_waits_naming_the_pair(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a"), leaf("b")]), &[]);
    let profile = profile(&[("build", 1)], cx);
    let started = start(&harness, start_request(), cx).await;
    let pair = "build-1 target and build-2 target resolve to /x/target";
    *harness.host.overlap.borrow_mut() = Some(vec![Problem {
        requirement: "lane locations do not overlap".to_string(),
        message: pair.to_string(),
        hint: None,
    }]);

    set_count(&profile, "build", 2, cx);
    cx.update(|cx| dispatcher(&started).pick_now(cx));
    cx.run_until_parked();
    let held = harness.lane("build", 2);
    assert_eq!(held.state, LaneState::Waiting);
    assert_eq!(held.reason, Some(WaitReason::Environment(pair.to_string())));
    assert_eq!(held.job, None);
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);

    *harness.host.overlap.borrow_mut() = None;
    set_count(&profile, "build", 3, cx);
    assert_eq!(harness.lane("build", 2).state, LaneState::Running);
    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
}

fn stamp_of(tasks: Vec<bongos_lanes_model::PickTask>, table: &str) -> StampFile {
    let table = parse(&format!("schema = 1\n{table}")).unwrap();
    StampFile::from_stamp(&stamp::compute(&graph(tasks), &table))
}

// With a recorded stamp the graph no longer matches, the lanes wait naming the change;
// once the accepted stamp is at the verified commit, the next pick proceeds. Without a stamp the
// check is off.
#[gpui::test]
async fn a_stale_stamp_holds_the_lanes_until_the_accepted_stamp_lands(cx: &mut TestAppContext) {
    let table = builds(1);
    let harness = harness(&table, graph(vec![leaf("a")]), &[]);
    *harness.host.stamp.borrow_mut() = Some(stamp_of(vec![leaf("a"), leaf("x")], &table));
    let started = start(&harness, start_request(), cx).await;

    let waiting = harness.lane("build", 1);
    assert_eq!(waiting.state, LaneState::Idle);
    assert!(
        matches!(&waiting.reason, Some(WaitReason::StaleGraph { diff }) if !diff.removed.is_empty()),
        "{:?}",
        waiting.reason
    );
    assert!(harness.engine.job_tasks().is_empty());

    *harness.host.stamp.borrow_mut() = Some(stamp_of(vec![leaf("a")], &table));
    cx.update(|cx| dispatcher(&started).verified_commit_changed("c2".to_string(), cx));
    cx.run_until_parked();
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    // Without stamp.toml the check is off.
    let unchecked = super::harness(&table, graph(vec![leaf("a")]), &[]);
    let _started = start(&unchecked, start_request(), cx).await;
    assert_eq!(unchecked.engine.job_tasks(), ["a"]);
}
