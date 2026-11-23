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
pub(super) struct CountProfile {
    counts: Mutex<HashMap<String, u32>>,
}

impl LaneProfileSource for CountProfile {
    fn count(&self, _project: &str, kind: &str) -> Option<u32> {
        self.counts.lock().unwrap().get(kind).copied()
    }
}

pub(super) fn profile(counts: &[(&str, u32)], cx: &mut TestAppContext) -> Arc<CountProfile> {
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
pub(super) fn set_count(profile: &CountProfile, kind: &str, count: u32, cx: &mut TestAppContext) {
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

fn table_of(table: &str) -> bongos_lanes_model::LaneTable {
    parse(&format!("schema = 1\n{table}")).unwrap()
}

/// What `load` reads at the shown commit: the graph and table, with only the waiting limit and
/// the stamp in its options.
fn loaded(harness: &Harness, table: &str) -> crate::read_modes::Loaded {
    crate::read_modes::Loaded {
        graph: harness.host.graph.borrow().clone(),
        table: table_of(table),
        options: bongos_lanes_model::PickOptions {
            waiting_limit: Some(10),
            ..bongos_lanes_model::PickOptions::default()
        },
    }
}

fn pick_read(harness: &Harness) -> crate::read_modes::PickRead {
    harness
        .writer
        .read(crate::read_modes::read_pick_state)
        .unwrap()
}

/// The reasons the dry run gives for `lane` doing nothing; `None` when it would start a job.
fn dry_reasons(
    loaded: &crate::read_modes::Loaded,
    read: &crate::read_modes::PickRead,
    options: &bongos_lanes_model::PickOptions,
    lane: &str,
) -> Option<Vec<WaitReason>> {
    let dry = bongos_lanes_model::explain(&loaded.graph, &read.state, &loaded.table, options);
    let lane = bongos_lanes_model::LaneId::parse(lane).unwrap();
    let (_, outcome) = dry
        .per_lane
        .into_iter()
        .find(|(id, _)| *id == lane)
        .unwrap_or_else(|| panic!("the dry run has no {lane}"));
    match outcome {
        bongos_lanes_model::PickOutcome::Nothing { reasons, .. } => {
            Some(reasons.into_iter().map(|waiting| waiting.reason).collect())
        }
        _ => None,
    }
}

// The dry run of the process that runs the dispatcher reads the dispatcher's own options: a lane
// the dispatcher leaves idle at its workflow's instance limit shows that limit in the dry run,
// and the bound the dispatcher started with applies. With no dispatcher, the same options are
// built from the run engine.
#[gpui::test]
async fn a_dry_run_reads_the_options_the_dispatcher_picks_with(cx: &mut TestAppContext) {
    let table = builds(2);
    let harness = harness(&table, graph(vec![leaf("a"), leaf("b")]), &[("w", 1)]);
    let request = crate::StartLanes {
        bound: Some(bongos_lanes_model::PickBound::Until(TaskId::new("b"))),
        ..start_request()
    };
    let started = start(&harness, request, cx).await;
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    let loaded = loaded(&harness, &table);
    let read = pick_read(&harness);

    let options = crate::read_modes::current_options(
        &crate::read_modes::OptionSources {
            dispatcher: Some(dispatcher(&started)),
            engine: None,
            profile: None,
            project: "project-1",
        },
        &loaded,
        &read,
    );
    assert_eq!(options.waiting_limit, Some(10));
    assert_eq!(
        options.bound,
        Some(bongos_lanes_model::PickBound::Until(TaskId::new("b")))
    );
    assert_eq!(options.counts.get("build"), Some(&2));
    assert_eq!(options.instances["w"].limit, Some(1));
    let reasons = dry_reasons(&loaded, &read, &options, "build-2").unwrap();
    assert!(
        reasons
            .iter()
            .any(|reason| matches!(reason, WaitReason::InstanceLimit { limit: 1, .. })),
        "{reasons:?}"
    );

    // With no dispatcher here, the run engine of this process answers the same use.
    let engine: &dyn crate::dispatcher::JobEngine = &*harness.engine;
    let options = crate::read_modes::current_options(
        &crate::read_modes::OptionSources {
            dispatcher: None,
            engine: Some(engine),
            profile: None,
            project: "project-1",
        },
        &loaded,
        &read,
    );
    assert_eq!(
        options.bound,
        Some(bongos_lanes_model::PickBound::Until(TaskId::new("b")))
    );
    let reasons = dry_reasons(&loaded, &read, &options, "build-2").unwrap();
    assert!(
        reasons
            .iter()
            .any(|reason| matches!(reason, WaitReason::InstanceLimit { limit: 1, .. })),
        "{reasons:?}"
    );

    // The options `load` alone gives know no limit, so the dry run would start b.
    assert_eq!(
        dry_reasons(&loaded, &read, &loaded.options, "build-2"),
        None
    );
}

// In a process with no dispatcher the dry run reads the profile's counts over the table's and
// the bound of a dispatcher record whose run has not ended.
#[gpui::test]
async fn a_dry_run_with_no_dispatcher_reads_the_profile_counts_and_the_recorded_bound(
    cx: &mut TestAppContext,
) {
    let table = builds(1);
    let harness = harness(&table, graph(vec![leaf("a")]), &[]);
    let _profile = profile(&[("build", 3)], cx);
    let mut record = crate::dispatcher::DispatcherRecord {
        run_id: "0190other".to_string(),
        host: "other-host".to_string(),
        started: crate::test_support::SINCE.to_string(),
        by: bongos_lanes_model::Author::System,
        bound: Some(crate::dispatcher::RecordedBound::Until(TaskId::new("a"))),
        unattended: None,
        definition_commit: None,
        table_commit: None,
        table_wait: None,
        stopped_by: None,
        ended: None,
    };
    harness
        .writer
        .transact(|tx| crate::dispatcher::put_dispatcher(tx, &record))
        .unwrap();
    let loaded = loaded(&harness, &table);
    let read = pick_read(&harness);

    let options = cx.update(|cx| crate::read_modes::options_now(&loaded, &read, "project-1", cx));
    assert_eq!(options.counts.get("build"), Some(&3));
    assert_eq!(
        options.bound,
        Some(bongos_lanes_model::PickBound::Until(TaskId::new("a")))
    );
    let dry = bongos_lanes_model::explain(&loaded.graph, &read.state, &loaded.table, &options);
    assert_eq!(dry.per_lane.len(), 3);

    // Once the recorded run ended its bound no longer applies.
    record.ended = Some(crate::lane_control::RunEnd::Succeeded);
    harness
        .writer
        .transact(|tx| crate::dispatcher::put_dispatcher(tx, &record))
        .unwrap();
    let read = pick_read(&harness);
    let options = cx.update(|cx| crate::read_modes::options_now(&loaded, &read, "project-1", cx));
    assert_eq!(options.bound, None);
}
