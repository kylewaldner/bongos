use std::collections::BTreeMap;

use crate::state::*;

fn lane(kind: &str, index: u32) -> LaneId {
    LaneId::new(kind, index)
}

fn task(id: &str) -> TaskId {
    TaskId::new(id)
}

fn job(run: &str) -> JobRecord {
    JobRecord {
        run_id: run.to_string(),
        subject: RunSubject::Task(task("t1")),
        workflow: "build".to_string(),
        started: "t0".to_string(),
        definition_commit: None,
    }
}

fn owner(state: OwnerState, lane: Option<LaneId>) -> OwnerRecord {
    OwnerRecord {
        lane,
        state,
        since: "t0".to_string(),
        by: Author::System,
        reason: None,
        prepared_commit: None,
        refreshed: false,
    }
}

fn lane_rec(id: &LaneId, state: LaneState) -> LaneRecord {
    LaneRecord::new(id, state)
}

fn running_lane() -> LaneRecord {
    let mut rec = lane_rec(&lane("build", 1), LaneState::Running);
    rec.job = Some(job("abcdef123456"));
    rec
}

fn input(outcome: JobEnd) -> JobEndInput<'static> {
    JobEndInput {
        outcome,
        lane_kind_is_ahead: false,
        all_leaves_finished: false,
        checks_owed: false,
        prepared_commit: None,
        now: "t1",
    }
}

#[test]
fn success_keeps_owner_active() {
    let o = owner(OwnerState::Active, Some(lane("build", 1)));
    let t = owner_after_job_end(&o, &running_lane(), &input(JobEnd::Succeeded));
    assert_eq!(t.owner.state, OwnerState::Active);
    assert_eq!(t.lane.state, LaneState::Idle);
    assert_eq!(t.lane.jobs_done, 1);
    assert!(t.lane.job.is_none());
}

#[test]
fn success_with_all_leaves_finished_is_done() {
    let o = owner(OwnerState::Active, Some(lane("build", 1)));
    let mut i = input(JobEnd::Succeeded);
    i.all_leaves_finished = true;
    let t = owner_after_job_end(&o, &running_lane(), &i);
    assert_eq!(t.owner.state, OwnerState::Done);
}

#[test]
fn done_is_withheld_while_checks_are_owed() {
    let o = owner(OwnerState::Active, Some(lane("build", 1)));
    let mut i = input(JobEnd::Succeeded);
    i.all_leaves_finished = true;
    i.checks_owed = true;
    let t = owner_after_job_end(&o, &running_lane(), &i);
    assert_eq!(t.owner.state, OwnerState::Active);
}

#[test]
fn ahead_success_prepares_and_clears_lane() {
    let o = owner(OwnerState::Ahead, Some(lane("build", 1)));
    let mut i = input(JobEnd::Succeeded);
    i.lane_kind_is_ahead = true;
    i.prepared_commit = Some("c0ffee".to_string());
    let t = owner_after_job_end(&o, &running_lane(), &i);
    assert_eq!(t.owner.state, OwnerState::Prepared);
    assert_eq!(t.owner.lane, None);
    assert_eq!(t.owner.prepared_commit.as_deref(), Some("c0ffee"));
}

#[test]
fn failed_and_halted_halt_lane_and_owner_keeping_job() {
    for end in [
        JobEnd::Failed("boom".to_string()),
        JobEnd::Halted("boom".to_string()),
    ] {
        let o = owner(OwnerState::Active, Some(lane("build", 1)));
        let t = owner_after_job_end(&o, &running_lane(), &input(end));
        assert_eq!(t.lane.state, LaneState::Halted);
        assert_eq!(t.owner.state, OwnerState::Halted);
        assert_eq!(t.owner.reason.as_deref(), Some("boom"));
        assert_eq!(t.lane.reason, Some(WaitReason::Failed("boom".to_string())));
        assert!(t.lane.job.is_some());
        assert_eq!(t.lane.jobs_done, 0);
        assert_eq!(t.owner.lane, Some(lane("build", 1)));
    }
}

#[test]
fn cancelled_pauses_lane_and_keeps_owner() {
    let o = owner(OwnerState::Active, Some(lane("build", 1)));
    let t = owner_after_job_end(&o, &running_lane(), &input(JobEnd::Cancelled));
    assert_eq!(t.lane.state, LaneState::Paused);
    assert_eq!(
        t.lane.reason,
        Some(WaitReason::Other("job cancelled".to_string()))
    );
    assert_eq!(t.owner, o);
}

#[test]
fn waiting_on_you_changes_nothing() {
    let o = owner(OwnerState::Active, Some(lane("build", 1)));
    let t = owner_after_job_end(&o, &running_lane(), &input(JobEnd::WaitingOnYou));
    assert_eq!(t.lane, running_lane());
    assert_eq!(t.owner, o);
}

#[test]
fn retry_resumes_halted_lane() {
    let o = owner(OwnerState::Active, Some(lane("build", 1)));
    let halted = owner_after_job_end(
        &o,
        &running_lane(),
        &input(JobEnd::Failed("boom".to_string())),
    );
    let t = lane_after_retry(&halted.owner, &halted.lane, "t2").unwrap();
    assert_eq!(t.lane.state, LaneState::Running);
    assert_eq!(t.lane.reason, None);
    assert_eq!(t.owner.state, OwnerState::Active);
    assert_eq!(t.owner.reason, None);
    assert_eq!(
        t.history.as_deref(),
        Some("resumed by retry of run abcdef12")
    );
    assert!(lane_after_retry(&o, &running_lane(), "t2").is_none());
}

#[test]
fn stop_running_lane_keeps_running() {
    let l = stop_lane(&running_lane(), StopCause::Stop);
    assert_eq!(l.state, LaneState::Running);
    assert_eq!(l.stop_requested, Some(StopCause::Stop));
}

#[test]
fn stop_idle_and_paused_stop_at_once() {
    for state in [LaneState::Idle, LaneState::Paused] {
        let l = stop_lane(&lane_rec(&lane("build", 1), state), StopCause::Stop);
        assert_eq!(l.state, LaneState::Stopped);
    }
}

#[test]
fn stop_leaves_halted_and_stopped_alone() {
    for state in [LaneState::Halted, LaneState::Stopped] {
        let rec = lane_rec(&lane("build", 1), state);
        assert_eq!(stop_lane(&rec, StopCause::Stop), rec);
    }
}

#[test]
fn stop_after_job_stops_lane_and_owner() {
    let o = owner(OwnerState::Active, Some(lane("build", 1)));
    let l = stop_lane(&running_lane(), StopCause::Stop);
    let t = owner_after_job_end(&o, &l, &input(JobEnd::Succeeded));
    let (l2, o2) = stop_at_job_boundary(&t.lane, Some(&t.owner), "t3");
    assert_eq!(l2.state, LaneState::Stopped);
    assert_eq!(l2.stop_requested, None);
    let o2 = o2.unwrap();
    assert_eq!(o2.state, OwnerState::Stopped);
    assert_eq!(o2.lane, Some(lane("build", 1)));
}

#[test]
fn stop_never_changes_halted_owner() {
    let o = owner(OwnerState::Halted, Some(lane("build", 1)));
    assert_eq!(owner_after_stop(&o, StopCause::Stop, "t3"), o);
    let mut l = lane_rec(&lane("build", 1), LaneState::Halted);
    l.stop_requested = Some(StopCause::Stop);
    let (l2, o2) = stop_at_job_boundary(&l, Some(&o), "t3");
    assert_eq!(l2.state, LaneState::Halted);
    assert_eq!(o2, Some(o));
}

fn state_with(lanes: &[(LaneId, LaneState)], owners: &[(&str, OwnerRecord)]) -> LanesState {
    let mut s = LanesState::default();
    for (id, st) in lanes {
        s.lanes.insert(id.clone(), lane_rec(id, *st));
    }
    for (t, o) in owners {
        s.owners.insert(task(t), o.clone());
    }
    s
}

fn counts(kind: &str, n: u32) -> BTreeMap<String, u32> {
    BTreeMap::from([(kind.to_string(), n)])
}

#[test]
fn reconcile_creates_missing_lanes() {
    let s = state_with(&[(lane("build", 1), LaneState::Idle)], &[]);
    let plan = reconcile_counts(&counts("build", 3), &s);
    assert_eq!(plan.create, vec![lane("build", 2), lane("build", 3)]);
    assert!(plan.lower.is_empty());
}

#[test]
fn reconcile_lowers_idle_now_and_running_after_job() {
    let s = state_with(
        &[
            (lane("build", 1), LaneState::Running),
            (lane("build", 2), LaneState::Idle),
            (lane("build", 3), LaneState::Running),
        ],
        &[],
    );
    let plan = reconcile_counts(&counts("build", 1), &s);
    assert_eq!(plan.lower, vec![lane("build", 2), lane("build", 3)]);
    assert_eq!(plan.remove_now, vec![lane("build", 2)]);
    assert_eq!(plan.stop_after_job, vec![lane("build", 3)]);
}

#[test]
fn lowered_running_lane_frees_task_once_gone() {
    let id = lane("build", 2);
    let o = owner(OwnerState::Active, Some(id.clone()));
    let mut s = state_with(&[(id.clone(), LaneState::Running)], &[("t1", o.clone())]);
    s.lanes.get_mut(&id).unwrap().job = Some(job("run1"));
    let plan = reconcile_counts(&counts("build", 1), &s);
    assert_eq!(plan.stop_after_job, vec![id.clone()]);
    assert!(plan.adoptable.is_empty());

    // The job ends: the lane stops and the owner stops with it.
    let l = stop_lane(&s.lanes[&id], StopCause::Count);
    let t = owner_after_job_end(&o, &l, &input(JobEnd::Succeeded));
    let (l2, o2) = stop_at_job_boundary(&t.lane, Some(&t.owner), "t4");
    s.lanes.insert(id.clone(), l2);
    s.owners.insert(task("t1"), o2.unwrap());
    let plan = reconcile_counts(&counts("build", 1), &s);
    assert_eq!(plan.remove_now, vec![id]);
    assert_eq!(plan.adoptable, vec![task("t1")]);

    // Once the lane is gone the task stays adoptable.
    s.lanes.clear();
    let plan = reconcile_counts(&counts("build", 1), &s);
    assert_eq!(plan.adoptable, vec![task("t1")]);
}

#[test]
fn stopped_owner_with_existing_lane_is_resumed_only_by_it() {
    let o = owner(OwnerState::Stopped, Some(lane("build", 1)));
    assert!(owner_adoptable_by(&o, true, &lane("build", 1)));
    assert!(!owner_adoptable_by(&o, true, &lane("build", 2)));
    assert!(owner_adoptable_by(&o, false, &lane("build", 2)));
    let s = state_with(&[(lane("build", 1), LaneState::Stopped)], &[("t1", o)]);
    assert!(
        reconcile_counts(&counts("build", 1), &s)
            .adoptable
            .is_empty()
    );
    let active = owner(OwnerState::Active, Some(lane("build", 1)));
    assert!(!owner_adoptable_by(&active, false, &lane("build", 2)));
}

fn request(task_id: &str, to: LaneId) -> AdoptRequest {
    AdoptRequest {
        task: task(task_id),
        lane: to,
        reason: "unstick it".to_string(),
        by: Author::Person("kyle".to_string()),
        since: "t5".to_string(),
        old_tree_readable: true,
        confirm_drop: false,
    }
}

fn adopt_state(owner_rec: OwnerRecord, source: LaneState) -> LanesState {
    let mut s = state_with(
        &[
            (lane("build", 1), source),
            (lane("build", 2), LaneState::Idle),
        ],
        &[("t1", owner_rec)],
    );
    s.lanes.get_mut(&lane("build", 1)).unwrap().job = Some(job("run123456789"));
    s
}

#[test]
fn adopt_refuses_running_owner() {
    let s = adopt_state(
        owner(OwnerState::Active, Some(lane("build", 1))),
        LaneState::Running,
    );
    let err = adopt(&s, &request("t1", lane("build", 2)), None).unwrap_err();
    assert_eq!(err.to_string(), "t1 is owned by build-1, which is running");
}

#[test]
fn adopt_refuses_other_states() {
    for st in [OwnerState::Ahead, OwnerState::Prepared, OwnerState::Done] {
        let s = adopt_state(owner(st, None), LaneState::Idle);
        let err = adopt(&s, &request("t1", lane("build", 2)), None).unwrap_err();
        assert!(matches!(err, AdoptRefusal::NotAdoptable { .. }), "{err}");
    }
    let s = adopt_state(
        owner(OwnerState::Active, Some(lane("build", 1))),
        LaneState::Paused,
    );
    assert!(matches!(
        adopt(&s, &request("t1", lane("build", 2)), None).unwrap_err(),
        AdoptRefusal::NotAdoptable { .. }
    ));
    let s = adopt_state(owner(OwnerState::Stopped, None), LaneState::Idle);
    assert!(matches!(
        adopt(&s, &request("nope", lane("build", 2)), None).unwrap_err(),
        AdoptRefusal::UnknownTask(_)
    ));
}

#[test]
fn adopt_refuses_pin_elsewhere() {
    let s = adopt_state(
        owner(OwnerState::Stopped, Some(lane("build", 1))),
        LaneState::Stopped,
    );
    let by_lane = PinTarget {
        lane: Some("build-1".to_string()),
        kind: None,
    };
    let by_kind = PinTarget {
        lane: None,
        kind: Some("review".to_string()),
    };
    for pin in [by_lane, by_kind] {
        assert!(matches!(
            adopt(&s, &request("t1", lane("build", 2)), Some(&pin)).unwrap_err(),
            AdoptRefusal::PinnedElsewhere { .. }
        ));
    }
    let ok = PinTarget {
        lane: None,
        kind: Some("build".to_string()),
    };
    assert!(adopt(&s, &request("t1", lane("build", 2)), Some(&ok)).is_ok());
}

#[test]
fn adopt_refuses_busy_target() {
    let mut s = adopt_state(
        owner(OwnerState::Stopped, Some(lane("build", 1))),
        LaneState::Stopped,
    );
    s.owners.insert(
        task("t2"),
        owner(OwnerState::Active, Some(lane("build", 2))),
    );
    assert!(matches!(
        adopt(&s, &request("t1", lane("build", 2)), None).unwrap_err(),
        AdoptRefusal::LaneBusy { .. }
    ));
    s.owners.remove(&task("t2"));
    s.lanes.get_mut(&lane("build", 2)).unwrap().state = LaneState::Paused;
    assert!(matches!(
        adopt(&s, &request("t1", lane("build", 2)), None).unwrap_err(),
        AdoptRefusal::LaneBusy { .. }
    ));
}

#[test]
fn adopt_refuses_blank_reason_and_unknown_lane() {
    let s = adopt_state(
        owner(OwnerState::Stopped, Some(lane("build", 1))),
        LaneState::Stopped,
    );
    let mut req = request("t1", lane("build", 2));
    req.reason = "  ".to_string();
    assert_eq!(
        adopt(&s, &req, None).unwrap_err(),
        AdoptRefusal::BlankReason
    );
    let req = request("t1", lane("build", 9));
    assert!(matches!(
        adopt(&s, &req, None).unwrap_err(),
        AdoptRefusal::UnknownLane(_)
    ));
}

#[test]
fn adopt_stopped_task_succeeds() {
    let s = adopt_state(
        owner(OwnerState::Stopped, Some(lane("build", 1))),
        LaneState::Stopped,
    );
    let out = adopt(&s, &request("t1", lane("build", 2)), None).unwrap();
    assert_eq!(out.owner.state, OwnerState::Active);
    assert_eq!(out.owner.lane, Some(lane("build", 2)));
    assert_eq!(out.owner.by, Author::Person("kyle".to_string()));
    assert_eq!(
        out.history,
        "adopted by build-2 from build-1: unstick it (kyle)"
    );
    let (id, source) = out.source_lane.unwrap();
    assert_eq!(id, lane("build", 1));
    assert_eq!(source.state, LaneState::Idle);
    assert!(source.job.is_none());
    assert_eq!(out.cancel_run, None);
}

#[test]
fn adopt_halted_task_cancels_run() {
    let mut s = adopt_state(
        owner(OwnerState::Halted, Some(lane("build", 1))),
        LaneState::Halted,
    );
    s.lanes.get_mut(&lane("build", 1)).unwrap().stop_requested = Some(StopCause::Stop);
    let out = adopt(&s, &request("t1", lane("build", 2)), None).unwrap();
    assert_eq!(out.cancel_run.as_deref(), Some("run123456789"));
    assert_eq!(
        out.cancel_history.as_deref(),
        Some("run run12345 cancelled by adopt to build-2")
    );
    assert_eq!(out.source_lane.unwrap().1.stop_requested, None);
}

#[test]
fn adopt_unreadable_tree_needs_confirmation() {
    let s = adopt_state(
        owner(OwnerState::Halted, Some(lane("build", 1))),
        LaneState::Halted,
    );
    let mut req = request("t1", lane("build", 2));
    req.old_tree_readable = false;
    assert_eq!(
        adopt(&s, &req, None).unwrap_err(),
        AdoptRefusal::UnreadableTree(task("t1"))
    );
    req.confirm_drop = true;
    let out = adopt(&s, &req, None).unwrap();
    assert!(out.dropped_work);
    assert!(out.history.contains("dropped"));
}

#[test]
fn adopt_when_source_lane_is_gone() {
    let mut s = adopt_state(
        owner(OwnerState::Stopped, Some(lane("build", 7))),
        LaneState::Idle,
    );
    s.lanes.remove(&lane("build", 1));
    let out = adopt(&s, &request("t1", lane("build", 2)), None).unwrap();
    assert!(out.source_lane.is_none());
    assert!(out.history.contains("from build-7"));
}

#[test]
fn lane_id_parse_and_display() {
    let id = LaneId::parse("build-2").unwrap();
    assert_eq!(id, lane("build", 2));
    assert_eq!(id.to_string(), "build-2");
    assert_eq!(LaneId::parse("my-kind-10"), Some(lane("my-kind", 10)));
    assert_eq!(LaneId::parse("build"), None);
    assert_eq!(LaneId::parse("build-0"), None);
}

#[test]
fn serde_round_trips() {
    let o = OwnerRecord {
        lane: Some(lane("build", 1)),
        state: OwnerState::Halted,
        since: "t0".to_string(),
        by: Author::Lane(lane("review", 2)),
        reason: Some("boom".to_string()),
        prepared_commit: Some("abc".to_string()),
        refreshed: true,
    };
    let json = serde_json::to_string(&o).unwrap();
    assert_eq!(serde_json::from_str::<OwnerRecord>(&json).unwrap(), o);

    let mut l = running_lane();
    l.reason = Some(WaitReason::Area {
        tag: "ui".to_string(),
        with: task("t2"),
        where_: "pane".to_string(),
    });
    l.stop_requested = Some(StopCause::Rollout);
    l.bound = Some(Bound {
        after_task: task("t9"),
    });
    let json = serde_json::to_string(&l).unwrap();
    assert_eq!(serde_json::from_str::<LaneRecord>(&json).unwrap(), l);

    let s = adopt_state(o, LaneState::Halted);
    let json = serde_json::to_string(&s).unwrap();
    assert!(json.contains("\"build-1\""));
    assert_eq!(serde_json::from_str::<LanesState>(&json).unwrap(), s);
}

#[test]
fn records_parse_with_unknown_and_missing_fields() {
    let o: OwnerRecord =
        serde_json::from_str(r#"{"state":"active","since":"t0","by":"system","future":{"x":1}}"#)
            .unwrap();
    assert_eq!(o.lane, None);
    assert!(!o.refreshed);
    let l: LaneRecord =
        serde_json::from_str(r#"{"kind":"build","index":1,"state":"idle","new_field":[1,2]}"#)
            .unwrap();
    assert_eq!(l.jobs_done, 0);
    assert_eq!(l.stop_requested, None);
    let s: LanesState = serde_json::from_str(r#"{"extra":true}"#).unwrap();
    assert!(s.lanes.is_empty());
    let j: JobRecord = serde_json::from_str(
        r#"{"run_id":"r","subject":{"task":"t1"},"workflow":"w","started":"s","more":1}"#,
    )
    .unwrap();
    assert_eq!(j.definition_commit, None);
}

#[test]
fn toml_records_parse_with_unknown_fields() {
    let l: LaneRecord =
        toml::from_str("kind = \"build\"\nindex = 2\nstate = \"paused\"\nsomething_new = 3\n")
            .unwrap();
    assert_eq!(l.id(), lane("build", 2));
}
