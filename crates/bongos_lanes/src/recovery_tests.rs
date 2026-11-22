//! Recovery after a crash (when this process becomes the writer, and at `lanes: start`), over the
//! fake host and engine of the dispatcher tests. The fake engine stands for the run engine after
//! its load: a run with a node in flight at the crash is interrupted, a run handed over after a
//! drain is not.

use bongos_lanes_model::{
    Author, JobRecord, LaneId, LaneRecord, LaneState, LanesState, OwnerRecord, OwnerState,
    RunSubject, TaskId, parse,
};
use bongos_workflow::RunStatus;
use gpui::TestAppContext;

use super::{
    DISPATCHER_RUN, Harness, builds, graph, harness, leaf, start, start_request,
};
use crate::dispatcher::{DispatcherRecord, JobEngine as _, dispatcher_record, put_dispatcher};
use crate::lane_control::RunEnd;
use crate::recovery::{
    HOST_STOPPED, InterruptionRecord, Recovered, interruption_policy, read_interruption, recover,
};
use crate::settings::LanesOnInterruption;
use crate::store::{LANES_NAMESPACE, put_lane, put_owner};
use crate::test_support::SINCE;

/// The dispatcher run of the host that was killed.
const OLD_DISPATCHER: &str = "0190olddispatcher";
/// The host that was killed.
const DEAD_HOST: &str = "host-killed";
/// The job run of `build-1`, mid-node when the host was killed.
const JOB_RUN: &str = "0190jobrun";
/// The job run of the halted `build-3`.
const HALTED_RUN: &str = "0190haltedrun";

fn build(index: u32) -> LaneId {
    LaneId::new("build", index)
}

fn job(run: &str, task: &str) -> JobRecord {
    JobRecord {
        run_id: run.to_string(),
        subject: RunSubject::Task(TaskId::new(task)),
        workflow: "w".to_string(),
        started: SINCE.to_string(),
        definition_commit: Some("c1".to_string()),
        extension: None,
    }
}

fn owner(lane: &LaneId, state: OwnerState) -> OwnerRecord {
    OwnerRecord {
        lane: Some(lane.clone()),
        state,
        since: SINCE.to_string(),
        by: Author::Lane(lane.clone()),
        reason: None,
        prepared_commit: None,
        refreshed: false,
    }
}

fn record_of(run: &str, host: &str) -> DispatcherRecord {
    DispatcherRecord {
        run_id: run.to_string(),
        host: host.to_string(),
        started: SINCE.to_string(),
        by: Author::System,
        bound: None,
        unattended: None,
        definition_commit: Some("c1".to_string()),
        table_commit: Some("c1".to_string()),
        table_wait: None,
        stopped_by: None,
        ended: None,
    }
}

/// The store and engine as a host killed mid-job leaves them, after the next writer's run load:
/// the dispatcher record names the dead host and its run; `build-1` runs task `a`, whose run had
/// a node in flight (interrupted, when `interrupted`); `build-2` is paused; `build-3` is halted
/// on task `c`.
fn killed_mid_job(table: &str, interrupted: bool) -> Harness {
    let harness = harness(table, graph(vec![leaf("a"), leaf("b"), leaf("c")]), &[]);
    harness
        .writer
        .transact(|tx| {
            put_dispatcher(tx, &record_of(OLD_DISPATCHER, DEAD_HOST))?;
            let mut running = LaneRecord::new(&build(1), LaneState::Running);
            running.job = Some(job(JOB_RUN, "a"));
            running.jobs_done = 3;
            put_lane(tx, &running)?;
            let mut paused = LaneRecord::new(&build(2), LaneState::Paused);
            paused.jobs_done = 1;
            put_lane(tx, &paused)?;
            let mut halted = LaneRecord::new(&build(3), LaneState::Halted);
            halted.job = Some(job(HALTED_RUN, "c"));
            put_lane(tx, &halted)?;
            put_owner(tx, &TaskId::new("a"), &owner(&build(1), OwnerState::Active))?;
            put_owner(tx, &TaskId::new("c"), &owner(&build(3), OwnerState::Halted))
        })
        .unwrap();
    let mut status = harness.engine.status.borrow_mut();
    status.insert(
        OLD_DISPATCHER.to_string(),
        ("lanes".to_string(), RunStatus::Running),
    );
    let job_status = if interrupted {
        RunStatus::WaitingOnYou
    } else {
        RunStatus::Running
    };
    status.insert(JOB_RUN.to_string(), ("w".to_string(), job_status));
    status.insert(
        HALTED_RUN.to_string(),
        ("w".to_string(), RunStatus::Halted),
    );
    drop(status);
    if interrupted {
        harness
            .engine
            .interrupted
            .borrow_mut()
            .insert(JOB_RUN.to_string());
    }
    harness
}

fn recover_now(harness: &Harness, policy: LanesOnInterruption, cx: &mut TestAppContext) -> Recovered {
    let recovered = cx.update(|cx| recover(&harness.lanes.parts, policy, cx).unwrap());
    cx.run_until_parked();
    recovered
}

fn dispatcher_of(harness: &Harness) -> DispatcherRecord {
    harness
        .writer
        .transact(dispatcher_record)
        .unwrap()
        .expect("a dispatcher record")
}

fn interruption_of(harness: &Harness, run: &str) -> Option<InterruptionRecord> {
    harness
        .writer
        .read(|read| read_interruption(read, run))
        .unwrap()
}

fn closed_with_host_stopped(harness: &Harness) {
    assert_eq!(
        *harness.engine.closes.borrow(),
        [
            format!("interrupt {OLD_DISPATCHER}"),
            format!("end {OLD_DISPATCHER} {HOST_STOPPED}"),
        ]
    );
    assert_eq!(
        *harness.engine.ends.borrow(),
        [(
            OLD_DISPATCHER.to_string(),
            RunEnd::Cancelled,
            HOST_STOPPED.to_string()
        )]
    );
}

// Acceptance: after killing the host mid-job, the next writer's recovery closes the dead host's
// dispatcher ("host stopped"), the interrupted run waits on you, and every owner and lane record
// is as it was; `lanes: start` starts a new dispatcher run and runs no job twice (the lanes keep
// their tasks, paused and halted lanes stay so); a later `lanes: start` re-adopts the live host.
#[gpui::test]
async fn after_killing_the_host_mid_job_lanes_start_readopts_and_the_interrupted_run_waits(
    cx: &mut TestAppContext,
) {
    let harness = killed_mid_job(&builds(3), true);
    let before: LanesState = harness.state();

    let recovered = recover_now(&harness, LanesOnInterruption::Ask, cx);

    assert_eq!(
        recovered,
        Recovered {
            closed_dispatcher: Some(OLD_DISPATCHER.to_string()),
            cleared: Vec::new(),
            resumed: Vec::new(),
            waiting: vec![JOB_RUN.to_string()],
        }
    );
    closed_with_host_stopped(&harness);
    assert_eq!(dispatcher_of(&harness).ended, Some(RunEnd::Cancelled));
    assert_eq!(harness.state(), before, "no owner or lane record changed");
    assert!(harness.engine.resumes.borrow().is_empty());
    assert_eq!(interruption_of(&harness, JOB_RUN), None);

    // Recovery again changes nothing more.
    let again = recover_now(&harness, LanesOnInterruption::Ask, cx);
    assert_eq!(again.closed_dispatcher, None);
    assert_eq!(harness.engine.closes.borrow().len(), 2);
    assert_eq!(harness.state(), before);

    let started = start(&harness, start_request(), cx).await;
    let dispatcher = started.dispatcher.expect("a new dispatcher runs here");
    assert!(!started.attached);
    assert_eq!(started.run_id, DISPATCHER_RUN);
    assert_eq!(harness.engine.dispatcher_starts.borrow().len(), 1);
    cx.update(|cx| dispatcher.pick_now(cx));
    cx.run_until_parked();

    assert!(
        harness.engine.jobs.borrow().is_empty(),
        "no job started again: {:?}",
        harness.engine.job_tasks()
    );
    let state = harness.state();
    assert_eq!(state.owners, before.owners);
    assert_eq!(state.lanes, before.lanes, "the lanes keep their tasks");
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(harness.lane("build", 2).state, LaneState::Paused);
    assert_eq!(harness.lane("build", 3).state, LaneState::Halted);
    assert!(harness.engine.interrupted.borrow().contains(JOB_RUN));
    assert_eq!(harness.engine.closes.borrow().len(), 2);

    // A second start finds this host live and attaches to its run.
    harness
        .host
        .live_hosts
        .borrow_mut()
        .insert("this-host".to_string());
    let readopted = start(&harness, start_request(), cx).await;
    assert!(readopted.attached);
    assert_eq!(readopted.run_id, DISPATCHER_RUN);
    assert_eq!(harness.engine.dispatcher_starts.borrow().len(), 1);
    assert!(harness.engine.jobs.borrow().is_empty());
}

// `lanes: start` right after the kill, with no recovery at writer start before it, closes the
// dead host's dispatcher record itself before it starts the new run.
#[gpui::test]
async fn lanes_start_closes_a_dead_hosts_dispatcher_with_host_stopped(cx: &mut TestAppContext) {
    let harness = killed_mid_job(&builds(3), true);
    let before = harness.state();

    let started = start(&harness, start_request(), cx).await;

    assert!(started.dispatcher.is_some());
    closed_with_host_stopped(&harness);
    assert_eq!(dispatcher_of(&harness).run_id, DISPATCHER_RUN);
    assert_eq!(dispatcher_of(&harness).ended, None);
    assert!(harness.engine.jobs.borrow().is_empty());
    assert_eq!(harness.state().lanes, before.lanes);
    assert_eq!(harness.state().owners, before.owners);
}

// PRD 32: with `on_interruption = resume` the interrupted run is resumed once, its interruption
// record written before the resume; a second interruption of the same run waits on you.
#[gpui::test]
async fn on_interruption_resume_resumes_once_and_a_second_interruption_waits(
    cx: &mut TestAppContext,
) {
    let table = format!("on_interruption = \"resume\"\n{}", builds(3));
    let harness = killed_mid_job(&table, true);
    let before = harness.state();

    let started = start(&harness, start_request(), cx).await;
    let _dispatcher = started.dispatcher.unwrap();

    assert_eq!(
        *harness.engine.resumes.borrow(),
        [(JOB_RUN.to_string(), Some(InterruptionRecord { resumed: 1 }))],
        "the record is committed before the resume"
    );
    assert_eq!(
        harness.engine.run_status(JOB_RUN),
        Some(RunStatus::Running)
    );
    assert!(harness.engine.jobs.borrow().is_empty(), "no job twice");
    assert_eq!(harness.state().lanes, before.lanes);
    assert_eq!(harness.state().owners, before.owners);

    // Recovery again: nothing is interrupted, nothing is resumed.
    let again = recover_now(&harness, LanesOnInterruption::Resume, cx);
    assert!(again.resumed.is_empty() && again.waiting.is_empty());
    assert_eq!(harness.engine.resumes.borrow().len(), 1);

    // The run is interrupted a second time (another crash): it waits on you.
    harness
        .engine
        .interrupted
        .borrow_mut()
        .insert(JOB_RUN.to_string());
    let second = recover_now(&harness, LanesOnInterruption::Resume, cx);
    assert_eq!(second.resumed, Vec::<String>::new());
    assert_eq!(second.waiting, [JOB_RUN.to_string()]);
    assert_eq!(harness.engine.resumes.borrow().len(), 1);
    assert_eq!(
        interruption_of(&harness, JOB_RUN),
        Some(InterruptionRecord { resumed: 1 })
    );
    assert!(harness.engine.interrupted.borrow().contains(JOB_RUN));
}

// With `ask` (the default) recovery resumes nothing and writes no interruption record.
#[gpui::test]
async fn on_interruption_ask_leaves_the_run_waiting(cx: &mut TestAppContext) {
    let table = format!("on_interruption = \"ask\"\n{}", builds(3));
    let harness = killed_mid_job(&table, true);

    let _started = start(&harness, start_request(), cx).await;

    assert!(harness.engine.resumes.borrow().is_empty());
    assert_eq!(interruption_of(&harness, JOB_RUN), None);
    assert!(harness.engine.interrupted.borrow().contains(JOB_RUN));
    assert_eq!(harness.lane("build", 1).job, Some(job(JOB_RUN, "a")));
}

// A crash right after `HandedOver`: the run engine continues the handed-over run (drained, not
// interrupted), so recovery resumes nothing, the lane keeps its running job and `lanes: start`
// starts no job twice.
#[gpui::test]
async fn a_crash_right_after_handed_over_continues_the_runs(cx: &mut TestAppContext) {
    let table = format!("on_interruption = \"resume\"\n{}", builds(3));
    let harness = killed_mid_job(&table, false);
    let before = harness.state();

    let recovered = recover_now(&harness, LanesOnInterruption::Resume, cx);
    assert!(recovered.resumed.is_empty() && recovered.waiting.is_empty());
    let started = start(&harness, start_request(), cx).await;
    let dispatcher = started.dispatcher.unwrap();
    cx.update(|cx| dispatcher.pick_now(cx));
    cx.run_until_parked();

    assert!(harness.engine.resumes.borrow().is_empty());
    assert_eq!(interruption_of(&harness, JOB_RUN), None);
    assert_eq!(
        harness.engine.run_status(JOB_RUN),
        Some(RunStatus::Running)
    );
    assert!(harness.engine.jobs.borrow().is_empty(), "no job twice");
    assert_eq!(harness.state().lanes, before.lanes);
    assert_eq!(harness.state().owners, before.owners);
}

// A crash between the pick and the run start, found at writer start: the job is cleared and the
// lane idle, its owner kept, so the same lane continues the task.
#[gpui::test]
async fn a_crash_between_pick_and_run_start_leaves_the_lane_idle_with_its_owner(
    cx: &mut TestAppContext,
) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    harness
        .writer
        .transact(|tx| {
            put_dispatcher(tx, &record_of(OLD_DISPATCHER, DEAD_HOST))?;
            let mut lane = LaneRecord::new(&build(1), LaneState::Running);
            lane.job = Some(job("0190neverstarted", "a"));
            put_lane(tx, &lane)?;
            put_owner(tx, &TaskId::new("a"), &owner(&build(1), OwnerState::Active))
        })
        .unwrap();

    let recovered = recover_now(&harness, LanesOnInterruption::Resume, cx);

    assert_eq!(recovered.cleared, [build(1)]);
    let lane = harness.lane("build", 1);
    assert_eq!((lane.state, lane.job), (LaneState::Idle, None));
    assert_eq!(
        harness.state().owners[&TaskId::new("a")],
        owner(&build(1), OwnerState::Active)
    );
    // The dispatcher's run was never loaded: it is marked ended, the engine is not asked.
    assert_eq!(recovered.closed_dispatcher, Some(OLD_DISPATCHER.to_string()));
    assert!(harness.engine.closes.borrow().is_empty());

    // The same lane takes its task again at the next start, once.
    let started = start(&harness, start_request(), cx).await;
    let _dispatcher = started.dispatcher.unwrap();
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
}

// While this process's own dispatcher runs (its record names a live host), recovery leaves the
// record and every reserved job alone: a start may be in flight.
#[gpui::test]
async fn recovery_leaves_a_live_hosts_dispatcher_and_its_reserved_jobs(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    harness
        .host
        .live_hosts
        .borrow_mut()
        .insert("this-host".to_string());
    harness
        .writer
        .transact(|tx| {
            put_dispatcher(tx, &record_of(OLD_DISPATCHER, "this-host"))?;
            let mut lane = LaneRecord::new(&build(1), LaneState::Running);
            lane.job = Some(job("0190starting", "a"));
            put_lane(tx, &lane)
        })
        .unwrap();
    let before = harness.state();

    let recovered = recover_now(&harness, LanesOnInterruption::Resume, cx);

    assert_eq!(recovered, Recovered::default());
    assert_eq!(harness.state(), before);
    assert_eq!(dispatcher_of(&harness).ended, None);
    assert!(harness.engine.closes.borrow().is_empty());
}

// First use: no dispatcher record and no lane record; recovery writes nothing and asks the
// engine nothing.
#[gpui::test]
async fn recovery_of_an_empty_state_does_nothing(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);

    let recovered = recover_now(&harness, LanesOnInterruption::Resume, cx);

    assert_eq!(recovered, Recovered::default());
    let keys = harness
        .writer
        .read(|read| read.kv(LANES_NAMESPACE).scan(""))
        .unwrap();
    assert!(keys.is_empty(), "{keys:?}");
    assert!(harness.engine.closes.borrow().is_empty());
    assert!(harness.engine.resumes.borrow().is_empty());
}

#[test]
fn lanes_toml_overrides_the_on_interruption_setting() {
    let table = |line: &str| parse(&format!("schema = 1\n{line}{}", builds(1))).unwrap();
    use LanesOnInterruption::{Ask, Resume};
    assert_eq!(interruption_policy(None, Resume), Resume);
    assert_eq!(interruption_policy(Some(&table("")), Ask), Ask);
    assert_eq!(
        interruption_policy(Some(&table("on_interruption = \"resume\"\n")), Ask),
        Resume
    );
    assert_eq!(
        interruption_policy(Some(&table("on_interruption = \"ask\"\n")), Resume),
        Ask
    );
    assert_eq!(
        interruption_policy(Some(&table("on_interruption = \"later\"\n")), Ask),
        Ask
    );
}
