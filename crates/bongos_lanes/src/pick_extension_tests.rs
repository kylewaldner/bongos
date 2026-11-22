//! Pick extensions in the dispatcher's picks and in the settling of the jobs they assign, over
//! the fake host and engine of the dispatcher tests, with a fake extension.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bongos_lanes_model::{
    AssignedWork, Assignment, JobRecord, LaneId, LaneRecord, LaneState, RankKey, RunSubject,
    WaitReason,
};
use bongos_project_repo::StateTx;
use bongos_workflow::lifecycle::RefusalKind;
use bongos_workflow::{Refusal, RunStatus};
use bongos_workflow_model::record::{EndState, Lifecycle};
use gpui::TestAppContext;

use super::{Harness, builds, graph, harness, leaf, start, start_request};
use crate::dispatch_job::DROPPED_BEFORE_START;
use crate::dispatcher::{JobEngine as _, NOT_STARTED_BEFORE_A_CRASH};
use crate::hooks::{BEFORE_JOB, BeforeJob, JobHooks};
use crate::lane_control::LaneTarget;
use crate::pick_extension::{
    ExtensionPick, JobChange, LaneAfterJob, PickExtension, PickExtensions, instance_room,
};
use crate::store::put_lane;
use crate::test_support::{GatedHook, SINCE};

/// The kind of the fake extension's work.
const QUEUE: &str = "queue";
/// The namespace the fake extension keeps its claims in.
const CLAIMS: &str = "queue-claims";

/// One call the fake extension heard.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Before(LaneId),
    After(LaneId),
    Changed(LaneId, String, JobChange),
}

/// A pick extension that answers what the test set, once per lane, and records every call.
#[derive(Default)]
struct FakeExtension {
    /// What `before_pick` answers for a lane, once; then `Pass`.
    before: RefCell<BTreeMap<LaneId, ExtensionPick>>,
    /// What `after_pick` answers for a lane, once; then `Pass`.
    after: RefCell<BTreeMap<LaneId, ExtensionPick>>,
    /// In `before_pick`, asks `instance_room` for this workflow and assigns a job of it when
    /// there is room, once per lane.
    assign_with_room: RefCell<Option<String>>,
    /// Every answer of `instance_room`, by lane.
    rooms: RefCell<Vec<(LaneId, Result<(), WaitReason>)>>,
    /// What `job_changed` answers.
    answer: RefCell<Option<LaneAfterJob>>,
    calls: RefCell<Vec<Call>>,
}

impl FakeExtension {
    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn changes(&self) -> Vec<(LaneId, String, JobChange)> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Changed(lane, run, change) => Some((lane, run, change)),
                _ => None,
            })
            .collect()
    }

    /// `before_pick` assigns a queue job (with `run_id`, when given) to `lane` once.
    fn assign_before(&self, lane: &LaneId, run_id: Option<&str>) {
        self.before
            .borrow_mut()
            .insert(lane.clone(), ExtensionPick::Assign(queue_job(run_id)));
    }
}

/// Writes the extension's claim of a job in the same transaction as its answer.
fn claim(tx: &mut StateTx, lane: &LaneId, pick: &ExtensionPick) {
    if let ExtensionPick::Assign(assignment) = pick {
        let run = assignment.run_id.clone().unwrap_or_default();
        tx.kv(CLAIMS)
            .put(&lane.to_string(), toml::Value::String(run))
            .unwrap();
    }
}

impl PickExtension for FakeExtension {
    fn before_pick(&self, tx: &mut StateTx, lane: &LaneId) -> ExtensionPick {
        self.calls.borrow_mut().push(Call::Before(lane.clone()));
        if let Some(workflow) = self.assign_with_room.borrow().clone()
            && !self.rooms.borrow().iter().any(|(seen, _)| seen == lane)
        {
            let room = instance_room(tx, &workflow);
            self.rooms.borrow_mut().push((lane.clone(), room.clone()));
            if room.is_ok() {
                return ExtensionPick::Assign(queue_job(None));
            }
        }
        let pick = self
            .before
            .borrow_mut()
            .remove(lane)
            .unwrap_or(ExtensionPick::Pass);
        claim(tx, lane, &pick);
        pick
    }

    fn after_pick(&self, tx: &mut StateTx, lane: &LaneId) -> ExtensionPick {
        self.calls.borrow_mut().push(Call::After(lane.clone()));
        let pick = self
            .after
            .borrow_mut()
            .remove(lane)
            .unwrap_or(ExtensionPick::Pass);
        claim(tx, lane, &pick);
        pick
    }

    fn job_changed(
        &self,
        _tx: &mut StateTx,
        lane: &LaneId,
        job: &JobRecord,
        change: JobChange,
    ) -> Option<LaneAfterJob> {
        self.calls
            .borrow_mut()
            .push(Call::Changed(lane.clone(), job.run_id.clone(), change));
        self.answer.borrow().clone()
    }
}

/// A queue job of workflow `w` with no task.
fn queue_job(run_id: Option<&str>) -> Assignment {
    Assignment {
        work: AssignedWork::Extension {
            kind: QUEUE.to_string(),
            task: None,
            payload: Default::default(),
        },
        workflow: "w".to_string(),
        inputs: Vec::new(),
        rank: RankKey { levels: Vec::new() },
        reason: "queue item".to_string(),
        owner_change: Vec::new(),
        refresh_first: false,
        run_id: run_id.map(str::to_string),
    }
}

fn build(index: u32) -> LaneId {
    LaneId::new("build", index)
}

fn register(cx: &mut TestAppContext, extension: &Arc<FakeExtension>) {
    let extension = extension.clone();
    cx.update(|cx| PickExtensions::register(extension, cx));
}

/// A registered fake extension whose `before_pick` assigns a queue job to `build-1` once.
fn assigning(cx: &mut TestAppContext) -> Arc<FakeExtension> {
    let extension = Arc::new(FakeExtension::default());
    extension.assign_before(&build(1), None);
    register(cx, &extension);
    extension
}

fn run_of_lane(harness: &Harness, index: u32) -> String {
    harness
        .lane("build", index)
        .job
        .expect("the lane holds a job")
        .run_id
}

fn hook_wait(message: &str) -> WaitReason {
    WaitReason::Hook {
        point: BEFORE_JOB.to_string(),
        message: message.to_string(),
    }
}

fn register_hook(cx: &mut TestAppContext, hook: &Arc<GatedHook>) {
    let hook = hook.clone();
    cx.update(|cx| JobHooks::register(hook, cx));
}

fn gated_before() -> Arc<GatedHook> {
    let hook = Arc::new(GatedHook::default());
    hook.gate_before.store(true, Ordering::SeqCst);
    hook
}

fn stop_all(harness: &Harness, cx: &mut TestAppContext) {
    cx.update(|cx| {
        harness
            .lanes
            .stop(
                LaneTarget::All,
                bongos_lanes_model::Author::Person("kim".to_string()),
                cx,
            )
            .unwrap()
    });
    cx.run_until_parked();
}

fn not_started(reason: &str) -> JobChange {
    JobChange::NotStarted {
        reason: reason.to_string(),
    }
}

#[gpui::test]
async fn before_pick_assign_with_a_run_id_starts_that_run(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let extension = Arc::new(FakeExtension::default());
    extension.assign_before(&build(1), Some("0190claimed"));
    register(cx, &extension);

    let _started = start(&harness, start_request(), cx).await;

    let jobs = harness.engine.jobs.borrow();
    assert_eq!(jobs.len(), 1);
    let (request, seen) = &jobs[0];
    assert_eq!(request.run_id.as_deref(), Some("0190claimed"));
    assert!(harness.engine.knows("0190claimed"));
    // The claim and the lane's job were committed together, before the run was asked for.
    let job = seen.lanes[&build(1)].job.clone().unwrap();
    assert_eq!(job.run_id, "0190claimed");
    assert_eq!(job.extension.as_deref(), Some(QUEUE));
    assert_eq!(job.subject, RunSubject::Other(QUEUE.to_string()));
    let claimed = harness
        .writer
        .read(|read| read.kv(CLAIMS).get("build-1"))
        .unwrap();
    assert_eq!(
        claimed,
        Some(toml::Value::String("0190claimed".to_string()))
    );
    // The picker was not asked: the leaf is still free.
    assert!(
        !harness
            .state()
            .owners
            .contains_key(&bongos_lanes_model::TaskId::new("a"))
    );
}

#[gpui::test]
async fn after_pick_is_called_only_after_the_picker_found_nothing(cx: &mut TestAppContext) {
    let harness = harness(&builds(3), graph(vec![leaf("a")]), &[]);
    let extension = assigning(cx);

    let _started = start(&harness, start_request(), cx).await;

    // build-1 took the extension's job, build-2 the picker's, build-3 found nothing.
    assert_eq!(
        harness.lane("build", 1).job.unwrap().extension.as_deref(),
        Some(QUEUE)
    );
    let mut tasks = harness.engine.job_tasks();
    tasks.sort();
    assert_eq!(tasks, ["", "a"]);
    let after: Vec<LaneId> = extension
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::After(lane) => Some(lane),
            _ => None,
        })
        .collect();
    assert!(!after.is_empty());
    assert!(after.iter().all(|lane| *lane == build(3)), "{after:?}");
}

#[gpui::test]
async fn after_pick_assigns_when_the_picker_found_nothing(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let extension = Arc::new(FakeExtension::default());
    extension.after.borrow_mut().insert(
        build(1),
        ExtensionPick::Assign(queue_job(Some("0190after"))),
    );
    register(cx, &extension);

    let _started = start(&harness, start_request(), cx).await;

    assert_eq!(run_of_lane(&harness, 1), "0190after");
    assert!(harness.engine.knows("0190after"));
    assert_eq!(
        extension.calls()[..2],
        [Call::Before(build(1)), Call::After(build(1))]
    );
}

#[gpui::test]
async fn stop_lane_stops_the_lane_with_the_extension_text(cx: &mut TestAppContext) {
    let harness = harness(&builds(2), graph(vec![leaf("a")]), &[]);
    let extension = Arc::new(FakeExtension::default());
    extension.before.borrow_mut().insert(
        build(1),
        ExtensionPick::StopLane("queue fixes: item exhausted".to_string()),
    );
    register(cx, &extension);

    let _started = start(&harness, start_request(), cx).await;

    let stopped = harness.lane("build", 1);
    assert_eq!(stopped.state, LaneState::Stopped);
    assert_eq!(stopped.stop_requested, None);
    assert_eq!(
        stopped.reason,
        Some(WaitReason::Other("queue fixes: item exhausted".to_string()))
    );
    assert_eq!(stopped.job, None);
    // The other lane picked as usual.
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert_eq!(harness.lane("build", 2).state, LaneState::Running);
}

#[gpui::test]
async fn instance_room_counts_an_assignment_made_earlier_in_the_pass(cx: &mut TestAppContext) {
    let harness = harness(&builds(2), graph(vec![]), &[("w", 1)]);
    harness.host.gated.set(true);
    let extension = Arc::new(FakeExtension::default());
    *extension.assign_with_room.borrow_mut() = Some("w".to_string());
    register(cx, &extension);

    let _started = start(&harness, start_request(), cx).await;

    let rooms = extension.rooms.borrow().clone();
    assert_eq!(rooms.len(), 2, "{rooms:?}");
    assert_eq!(rooms[0], (build(1), Ok(())));
    assert_eq!(rooms[1].0, build(2));
    assert!(
        matches!(
            &rooms[1].1,
            Err(WaitReason::InstanceLimit { workflow, active: 1, limit: 1, .. }) if workflow == "w"
        ),
        "{rooms:?}"
    );
    assert!(harness.lane("build", 1).job.is_some());
    assert_eq!(harness.lane("build", 2).job, None);
}

#[gpui::test]
async fn instance_room_counts_a_job_written_earlier_in_the_same_transaction(
    cx: &mut TestAppContext,
) {
    let harness = harness(&builds(1), graph(vec![]), &[("w", 1)]);
    let _started = start(&harness, start_request(), cx).await;
    let engine: std::rc::Rc<dyn crate::JobEngine> = harness.engine.clone();

    let rooms = crate::pick_extension::with_engine(&engine, || {
        harness.writer.transact(|tx| {
            let before = instance_room(tx, "w");
            let mut lane = LaneRecord::new(&build(1), LaneState::Running);
            lane.job = Some(JobRecord {
                run_id: "0190reserved".to_string(),
                subject: RunSubject::Other(QUEUE.to_string()),
                workflow: "w".to_string(),
                started: SINCE.to_string(),
                definition_commit: None,
                extension: Some(QUEUE.to_string()),
            });
            put_lane(tx, &lane)?;
            Ok((before, instance_room(tx, "w")))
        })
    })
    .unwrap();

    assert_eq!(rooms.0, Ok(()));
    assert!(
        matches!(rooms.1, Err(WaitReason::InstanceLimit { active: 1, .. })),
        "{rooms:?}"
    );
}

// With `instances = 1`, a job held in a pending `before_job` keeps a second lane from taking
// another job of the workflow, through the picker and through an extension alike.
#[gpui::test]
async fn a_job_pending_in_before_job_holds_its_instance(cx: &mut TestAppContext) {
    let harness = harness(&builds(2), graph(vec![leaf("a"), leaf("b")]), &[("w", 1)]);
    let hook = gated_before();
    register_hook(cx, &hook);
    let extension = Arc::new(FakeExtension::default());
    register(cx, &extension);

    let started = start(&harness, start_request(), cx).await;
    assert!(harness.lane("build", 1).job.is_some());
    assert!(harness.engine.job_tasks().is_empty());
    *extension.assign_with_room.borrow_mut() = Some("w".to_string());

    cx.update(|cx| started.dispatcher.as_ref().unwrap().pick_now(cx));
    cx.run_until_parked();

    let second = harness.lane("build", 2);
    assert_eq!(second.job, None);
    assert!(
        matches!(
            second.reason,
            Some(WaitReason::InstanceLimit {
                active: 1,
                limit: 1,
                ..
            })
        ),
        "{:?}",
        second.reason
    );
    let rooms = extension.rooms.borrow().clone();
    assert_eq!(rooms.len(), 1, "{rooms:?}");
    assert_eq!(rooms[0].0, build(2));
    assert!(
        matches!(rooms[0].1, Err(WaitReason::InstanceLimit { active: 1, .. })),
        "{rooms:?}"
    );
    assert!(harness.engine.job_tasks().is_empty());
}

#[gpui::test]
async fn a_before_job_refusal_tells_the_extension_not_started(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let hook = Arc::new(GatedHook::default());
    *hook.answer.lock().unwrap() = Some(BeforeJob::Refuse {
        reason: hook_wait("no slot"),
    });
    register_hook(cx, &hook);
    let extension = Arc::new(FakeExtension::default());
    extension.assign_before(&build(1), Some("0190refused"));
    register(cx, &extension);

    let _started = start(&harness, start_request(), cx).await;

    assert_eq!(
        extension.changes(),
        [(
            build(1),
            "0190refused".to_string(),
            not_started("before_job: no slot")
        )]
    );
    // No extension answered, so the default applies: the lane waits with the reason.
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.reason, Some(hook_wait("no slot")));
    assert_eq!(lane.job, None);
    assert!(harness.engine.job_tasks().is_empty());
}

#[gpui::test]
async fn a_preparation_failure_tells_the_extension_not_started(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    *harness.host.setup_error.borrow_mut() = Some(WaitReason::Environment("no space".to_string()));
    let extension = assigning(cx);
    *extension.answer.borrow_mut() = Some(LaneAfterJob::Idle);

    let _started = start(&harness, start_request(), cx).await;

    let changes = extension.changes();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].2, not_started("no space"));
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.job, None);
    assert!(harness.engine.jobs.borrow().is_empty());
}

// A crash between the pick and the start leaves a lane job with no run; the next start clears it
// and tells the extension, once.
#[gpui::test]
async fn a_job_left_unstarted_by_a_crash_is_cleared_at_the_next_start(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let mut lane = LaneRecord::new(&build(1), LaneState::Running);
    lane.job = Some(JobRecord {
        run_id: "0190crashed".to_string(),
        subject: RunSubject::Other(QUEUE.to_string()),
        workflow: "w".to_string(),
        started: SINCE.to_string(),
        definition_commit: None,
        extension: Some(QUEUE.to_string()),
    });
    harness.writer.transact(|tx| put_lane(tx, &lane)).unwrap();
    let extension = Arc::new(FakeExtension::default());
    register(cx, &extension);

    let started = start(&harness, start_request(), cx).await;
    cx.update(|cx| started.dispatcher.as_ref().unwrap().pick_now(cx));
    cx.run_until_parked();

    assert_eq!(
        extension.changes(),
        [(
            build(1),
            "0190crashed".to_string(),
            not_started(NOT_STARTED_BEFORE_A_CRASH)
        )]
    );
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.job, None);
    assert!(harness.engine.jobs.borrow().is_empty());
}

/// How an extension job goes wrong.
#[derive(Clone, Copy)]
enum Trouble {
    /// Its run fails.
    Fails,
    /// The run engine refuses its start (not for the machine's reasons).
    Refused,
}

/// The lane after an extension job of `build-1` ran into `trouble` and the extension answered
/// `answer`, with the change the extension heard.
async fn after_trouble(
    trouble: Trouble,
    answer: LaneAfterJob,
    cx: &mut TestAppContext,
) -> (LaneRecord, Vec<(LaneId, String, JobChange)>) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    if let Trouble::Refused = trouble {
        *harness.engine.refuse.borrow_mut() = Some(Refusal::new(
            RefusalKind::InstanceLimit,
            "The workflow is at its limit.",
            "Wait for a run to end.",
        ));
    }
    let extension = Arc::new(FakeExtension::default());
    extension.assign_before(&build(1), Some("0190trouble"));
    *extension.answer.borrow_mut() = Some(answer);
    register(cx, &extension);

    let _started = start(&harness, start_request(), cx).await;
    if let Trouble::Fails = trouble {
        cx.update(|cx| harness.engine.set("0190trouble", RunStatus::Failed, cx));
        cx.run_until_parked();
    }
    (harness.lane("build", 1), extension.changes())
}

#[gpui::test]
async fn a_failed_extension_job_leaves_the_lane_idle_when_the_extension_says_so(
    cx: &mut TestAppContext,
) {
    let (lane, changes) = after_trouble(Trouble::Fails, LaneAfterJob::Idle, cx).await;

    assert_eq!(
        changes,
        [(
            build(1),
            "0190trouble".to_string(),
            JobChange::Lifecycle(Lifecycle::Ended(EndState::Failed))
        )]
    );
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.job, None);
}

#[gpui::test]
async fn a_failed_extension_job_halts_the_lane_on_halt(cx: &mut TestAppContext) {
    let halt = LaneAfterJob::Halt("queue item failed".to_string());
    let (lane, changes) = after_trouble(Trouble::Fails, halt, cx).await;

    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(lane.state, LaneState::Halted);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Failed("queue item failed".to_string()))
    );
    // The failed run can be retried, so it stays the lane's job.
    assert_eq!(
        lane.job.map(|job| job.run_id).as_deref(),
        Some("0190trouble")
    );
}

#[gpui::test]
async fn a_refused_extension_job_leaves_the_lane_idle_when_the_extension_says_so(
    cx: &mut TestAppContext,
) {
    let (lane, changes) = after_trouble(Trouble::Refused, LaneAfterJob::Idle, cx).await;

    assert_eq!(
        changes,
        [(
            build(1),
            "0190trouble".to_string(),
            JobChange::Refused {
                reason: "The workflow is at its limit. Wait for a run to end.".to_string()
            }
        )]
    );
    // The default would halt the lane on this refusal.
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.job, None);
}

#[gpui::test]
async fn a_refused_extension_job_halts_the_lane_on_halt(cx: &mut TestAppContext) {
    let halt = LaneAfterJob::Halt("queue item refused".to_string());
    let (lane, changes) = after_trouble(Trouble::Refused, halt, cx).await;

    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(lane.state, LaneState::Halted);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Failed("queue item refused".to_string()))
    );
    assert_eq!(lane.job, None);
}

#[gpui::test]
async fn a_stop_with_before_job_pending_tells_the_extension_not_started(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let hook = gated_before();
    register_hook(cx, &hook);
    let extension = Arc::new(FakeExtension::default());
    extension.assign_before(&build(1), Some("0190pending"));
    register(cx, &extension);
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(run_of_lane(&harness, 1), "0190pending");

    stop_all(&harness, cx);

    assert_eq!(hook.dropped(), 1);
    assert_eq!(
        extension.changes(),
        [(
            build(1),
            "0190pending".to_string(),
            not_started(DROPPED_BEFORE_START)
        )]
    );
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Stopped);
    assert_eq!(lane.job, None);
    assert!(harness.engine.jobs.borrow().is_empty());
}

#[gpui::test]
async fn a_stop_after_go_before_the_run_tells_the_extension_not_started(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let hook = gated_before();
    register_hook(cx, &hook);
    let extension = Arc::new(FakeExtension::default());
    extension.assign_before(&build(1), Some("0190went"));
    register(cx, &extension);
    let _started = start(&harness, start_request(), cx).await;

    hook.decide(BeforeJob::Go);
    stop_all(&harness, cx);

    assert_eq!(
        extension.changes(),
        [(
            build(1),
            "0190went".to_string(),
            not_started(DROPPED_BEFORE_START)
        )]
    );
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Stopped);
    assert_eq!(lane.job, None);
    assert!(harness.engine.jobs.borrow().is_empty());
}

#[gpui::test]
async fn the_first_extension_that_answers_decides(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let silent = Arc::new(FakeExtension::default());
    silent.assign_before(&build(1), Some("0190first"));
    let halting = Arc::new(FakeExtension::default());
    *halting.answer.borrow_mut() = Some(LaneAfterJob::Halt("second decides".to_string()));
    let idling = Arc::new(FakeExtension::default());
    *idling.answer.borrow_mut() = Some(LaneAfterJob::Idle);
    for extension in [&silent, &halting, &idling] {
        register(cx, extension);
    }

    let _started = start(&harness, start_request(), cx).await;
    cx.update(|cx| harness.engine.set("0190first", RunStatus::Failed, cx));
    cx.run_until_parked();

    let failed = JobChange::Lifecycle(Lifecycle::Ended(EndState::Failed));
    assert_eq!(
        silent.changes(),
        [(build(1), "0190first".to_string(), failed.clone())]
    );
    assert_eq!(
        halting.changes(),
        [(build(1), "0190first".to_string(), failed)]
    );
    assert!(idling.changes().is_empty());
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Halted);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Failed("second decides".to_string()))
    );
}
