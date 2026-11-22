//! Lane wait handlers, the `lanes` needs-you source, the profile's `verify_lane` at a job's start
//! and the retry of a halted lane's run, over the fake host and engine of the dispatcher tests.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bongos_lanes_model::{LaneId, LaneState, OwnerState, Problem, TaskId, WaitReason, short_run};
use bongos_project_repo::StateEvent;
use bongos_work::NeedsYouRegistry;
use bongos_work_model::{fixtures, rollup};
use bongos_workflow::RunStatus;
use futures::channel::oneshot;
use gpui::{App, Task, TestAppContext};

use super::{Harness, builds, graph, harness, history_of, leaf, start, start_request};
use crate::Dispatcher;
use crate::lane_waits::{
    LaneWaitAction, LaneWaitHandler, LaneWaitHandlers, read_asks, register_needs_you,
};
use crate::profile::{LaneProfileSource, LaneProfiles};

/// One question a handler heard.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Heard {
    handler: &'static str,
    lane: LaneId,
    reason: WaitReason,
}

/// A handler that answers what the test set, at once or when the test says so, and records
/// every question in a log it may share with other handlers.
struct FakeHandler {
    name: &'static str,
    log: Rc<RefCell<Vec<Heard>>>,
    answer: RefCell<LaneWaitAction>,
    gated: Cell<bool>,
    gates: RefCell<Vec<oneshot::Sender<LaneWaitAction>>>,
}

impl FakeHandler {
    fn heard(&self) -> Vec<Heard> {
        self.log
            .borrow()
            .iter()
            .filter(|heard| heard.handler == self.name)
            .cloned()
            .collect()
    }

    /// Answers the oldest question still waiting with `action`; returns whether it was still
    /// listening.
    fn answer_first(&self, action: LaneWaitAction) -> bool {
        let gate = self.gates.borrow_mut().remove(0);
        gate.send(action).is_ok()
    }
}

impl LaneWaitHandler for FakeHandler {
    fn on_wait(&self, lane: &LaneId, reason: &WaitReason, cx: &mut App) -> Task<LaneWaitAction> {
        self.log.borrow_mut().push(Heard {
            handler: self.name,
            lane: lane.clone(),
            reason: reason.clone(),
        });
        if !self.gated.get() {
            return Task::ready(self.answer.borrow().clone());
        }
        let (sender, receiver) = oneshot::channel();
        self.gates.borrow_mut().push(sender);
        cx.foreground_executor()
            .spawn(async move { receiver.await.unwrap_or(LaneWaitAction::Keep) })
    }
}

fn handler_in(
    name: &'static str,
    log: &Rc<RefCell<Vec<Heard>>>,
    answer: LaneWaitAction,
    cx: &mut TestAppContext,
) -> Rc<FakeHandler> {
    let handler = Rc::new(FakeHandler {
        name,
        log: log.clone(),
        answer: RefCell::new(answer),
        gated: Cell::new(false),
        gates: RefCell::new(Vec::new()),
    });
    let registered: Rc<dyn LaneWaitHandler> = handler.clone();
    cx.update(|cx| LaneWaitHandlers::register(registered, cx));
    handler
}

/// A registered handler with its own log.
fn handler(answer: LaneWaitAction, cx: &mut TestAppContext) -> Rc<FakeHandler> {
    handler_in("only", &Rc::default(), answer, cx)
}

fn build(index: u32) -> LaneId {
    LaneId::new("build", index)
}

/// The wait of a lane whose disk is below its floor.
fn floor() -> WaitReason {
    WaitReason::Environment("the disk of build-1 is below its floor".to_string())
}

/// A started harness of one build lane and the ready leaf `task`, whose job's preparation fails
/// with the floor wait.
async fn waiting_on_floor(task: &str, cx: &mut TestAppContext) -> (Harness, Dispatcher) {
    let harness = harness(&builds(1), graph(vec![leaf(task)]), &[]);
    *harness.host.setup_error.borrow_mut() = Some(floor());
    let started = start(&harness, start_request(), cx).await;
    (harness, started.dispatcher.unwrap())
}

fn assert_waits_on_floor(harness: &Harness) {
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.reason, Some(floor()));
    assert_eq!(lane.job, None);
}

fn pass(dispatcher: &Dispatcher, cx: &mut TestAppContext) {
    cx.update(|cx| dispatcher.evaluate(false, cx));
    cx.run_until_parked();
}

fn asked(harness: &Harness) -> Option<String> {
    let status = harness.lanes.status().unwrap();
    status.lanes[0].asked.clone()
}

// the handler hears a lane enter a floor wait, and again at the next pass.
#[gpui::test]
async fn a_handler_hears_a_floor_wait_and_again_at_the_next_pass(cx: &mut TestAppContext) {
    let keep = handler(LaneWaitAction::Keep, cx);
    let (harness, dispatcher) = waiting_on_floor("a", cx).await;

    assert_waits_on_floor(&harness);
    let heard = keep.heard();
    assert!(!heard.is_empty());
    for question in &heard {
        assert_eq!((&question.lane, &question.reason), (&build(1), &floor()));
    }

    let before = heard.len();
    let passes = dispatcher.passes();
    pass(&dispatcher, cx);
    assert_eq!(dispatcher.passes(), passes + 1);
    assert_eq!(keep.heard().len(), before + 1);
    // Every pass asks at most once per lane.
    for _ in 0..3 {
        pass(&dispatcher, cx);
    }
    assert_eq!(keep.heard().len(), before + 4);
    assert_waits_on_floor(&harness);
}

// a question still pending is not asked again, however many passes run.
#[gpui::test]
async fn a_pending_question_is_not_asked_again(cx: &mut TestAppContext) {
    let slow = handler(LaneWaitAction::Keep, cx);
    slow.gated.set(true);
    let (harness, dispatcher) = waiting_on_floor("a", cx).await;
    assert_eq!(slow.heard().len(), 1);

    for _ in 0..3 {
        pass(&dispatcher, cx);
    }
    assert_eq!(slow.heard().len(), 1);

    assert!(slow.answer_first(LaneWaitAction::Keep));
    cx.run_until_parked();
    pass(&dispatcher, cx);
    assert_eq!(slow.heard().len(), 2);
    assert_waits_on_floor(&harness);
}

// `RecheckAfter(1 s)` evaluates the lane again after a second, and not before.
#[gpui::test]
async fn recheck_after_a_second_evaluates_the_lane_after_a_second(cx: &mut TestAppContext) {
    let _recheck = handler(LaneWaitAction::RecheckAfter(Duration::from_secs(1)), cx);
    let (harness, _dispatcher) = waiting_on_floor("a", cx).await;
    assert_waits_on_floor(&harness);
    // The disk has room again; nothing but the recheck looks at the lane before the timed pass.
    *harness.host.setup_error.borrow_mut() = None;

    cx.executor().advance_clock(Duration::from_millis(999));
    cx.run_until_parked();
    assert_waits_on_floor(&harness);
    assert!(harness.engine.job_tasks().is_empty());

    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
}

// A recheck that comes due after its lane left the wait changes nothing.
#[gpui::test]
async fn a_recheck_after_the_lane_left_its_wait_changes_nothing(cx: &mut TestAppContext) {
    let _recheck = handler(LaneWaitAction::RecheckAfter(Duration::from_secs(1)), cx);
    let (harness, _dispatcher) = waiting_on_floor("a", cx).await;
    *harness.host.setup_error.borrow_mut() = None;
    harness
        .lanes
        .pause(&build(1), &bongos_lanes_model::Author::System)
        .unwrap();
    cx.run_until_parked();
    let paused = harness.state();
    assert_eq!(paused.lanes[&build(1)].state, LaneState::Paused);

    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(harness.state(), paused);
    assert!(harness.engine.job_tasks().is_empty());
}

/// A work tree whose project holds the group `phase`, which holds the leaf `feature`.
fn work_tree() -> bongos_work_model::store::WorkTree {
    fixtures::tree(vec![fixtures::group(
        "phase",
        vec![fixtures::pr_ship("feature")],
    )])
}

// an ask shows on the lane row, on the task it owns and on that task's ancestors
// until `lanes: resume` clears it and evaluates the lane; no timed pass evaluates it meanwhile.
#[gpui::test]
async fn an_ask_needs_you_until_resume_clears_it(cx: &mut TestAppContext) {
    let tree = work_tree();
    let feature = fixtures::id_of(&tree, "feature");
    cx.update(register_needs_you);
    let ask = handler(LaneWaitAction::Ask("mount /mnt/fast".to_string()), cx);
    let (harness, _dispatcher) = waiting_on_floor(&feature, cx).await;

    assert_waits_on_floor(&harness);
    assert_eq!(asked(&harness).as_deref(), Some("mount /mnt/fast"));
    let found = cx.update(|cx| NeedsYouRegistry::collect(cx));
    assert_eq!(found.len(), 1);
    assert_eq!(found[&feature], ["build-1: mount /mnt/fast"]);
    // The Work panel's roll-up counts it on the task and every ancestor.
    let needs_you: HashSet<String> = found.keys().cloned().collect();
    for group in ["feature", "phase", "p"] {
        let task = fixtures::find(&tree, group);
        assert_eq!(
            rollup::rollup(task, &tree, &needs_you).needs_you,
            1,
            "{group}"
        );
    }
    let heard = ask.heard().len();

    // The disk has room again, but no timed pass looks at a lane waiting on a person.
    *harness.host.setup_error.borrow_mut() = None;
    for _ in 0..3 {
        cx.executor().advance_clock(Duration::from_secs(61));
        cx.run_until_parked();
    }
    assert_waits_on_floor(&harness);
    assert!(harness.engine.job_tasks().is_empty());
    assert_eq!(ask.heard().len(), heard);
    assert_eq!(asked(&harness).as_deref(), Some("mount /mnt/fast"));

    let said = harness.lanes.resume(&build(1)).unwrap();
    assert_eq!(said, "build-1 resumed");
    cx.run_until_parked();

    assert_eq!(harness.engine.job_tasks(), [feature.as_str()]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    assert_eq!(asked(&harness), None);
    assert!(cx.update(|cx| NeedsYouRegistry::collect(cx)).is_empty());
    assert!(harness.writer.read(read_asks).unwrap().is_empty());
}

// the handlers are asked in registration order and the first answer other than
// `Keep` decides; the later ones are not asked.
#[gpui::test]
async fn handlers_are_asked_in_order_until_one_decides(cx: &mut TestAppContext) {
    let log = Rc::default();
    let first = handler_in("first", &log, LaneWaitAction::Keep, cx);
    let second = handler_in(
        "second",
        &log,
        LaneWaitAction::Ask("free some space".to_string()),
        cx,
    );
    let third = handler_in("third", &log, LaneWaitAction::Keep, cx);
    let (harness, _dispatcher) = waiting_on_floor("a", cx).await;

    let order: Vec<&str> = log.borrow().iter().map(|heard| heard.handler).collect();
    assert_eq!(order, ["first", "second"]);
    assert_eq!(first.heard().len(), 1);
    assert_eq!(second.heard().len(), 1);
    assert!(third.heard().is_empty());
    assert_eq!(asked(&harness).as_deref(), Some("free some space"));
}

// with every handler keeping, each is asked in order at each pass, and the wait
// behaves as with none: the timed pass evaluates the lane again.
#[gpui::test]
async fn keeping_handlers_leave_the_wait_as_it_was(cx: &mut TestAppContext) {
    let log = Rc::default();
    let _first = handler_in("first", &log, LaneWaitAction::Keep, cx);
    let _second = handler_in("second", &log, LaneWaitAction::Keep, cx);
    let (harness, dispatcher) = waiting_on_floor("a", cx).await;

    log.borrow_mut().clear();
    pass(&dispatcher, cx);
    let order: Vec<&str> = log.borrow().iter().map(|heard| heard.handler).collect();
    assert_eq!(order, ["first", "second"]);

    *harness.host.setup_error.borrow_mut() = None;
    cx.executor().advance_clock(Duration::from_secs(60));
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["a"]);
}

// with no handler a wait behaves as before: no ask, and the timed pass evaluates
// the lane again.
#[gpui::test]
async fn with_no_handler_a_wait_behaves_as_before(cx: &mut TestAppContext) {
    let (harness, _dispatcher) = waiting_on_floor("a", cx).await;
    assert_waits_on_floor(&harness);
    assert_eq!(asked(&harness), None);
    assert!(harness.writer.read(read_asks).unwrap().is_empty());

    *harness.host.setup_error.borrow_mut() = None;
    cx.executor().advance_clock(Duration::from_secs(60));
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["a"]);
}

// An answer about a wait the lane left for another one is dropped; the new wait is asked about.
#[gpui::test]
async fn an_answer_about_an_earlier_wait_is_dropped(cx: &mut TestAppContext) {
    let slow = handler(LaneWaitAction::Keep, cx);
    slow.gated.set(true);
    let (harness, dispatcher) = waiting_on_floor("a", cx).await;
    assert_eq!(slow.heard().len(), 1);

    let other = WaitReason::Environment("the drive of build-1 is not mounted".to_string());
    *harness.host.setup_error.borrow_mut() = Some(other.clone());
    cx.update(|cx| dispatcher.pick_now(cx));
    cx.run_until_parked();
    assert_eq!(harness.lane("build", 1).reason, Some(other.clone()));
    let reasons: Vec<WaitReason> = slow.heard().into_iter().map(|heard| heard.reason).collect();
    assert_eq!(reasons, [floor(), other.clone()]);

    // The first question was dropped with its wait; its answer reaches nobody.
    assert!(!slow.answer_first(LaneWaitAction::Ask("about the floor".to_string())));
    cx.run_until_parked();
    assert_eq!(asked(&harness), None);

    assert!(slow.answer_first(LaneWaitAction::Ask("mount it".to_string())));
    cx.run_until_parked();
    assert_eq!(asked(&harness).as_deref(), Some("mount it"));
    let asks = harness.writer.read(read_asks).unwrap();
    assert_eq!(asks[&build(1)].reason, other);
}

// An answer that comes after its lane left the wait, before or after a pass saw it, is dropped.
#[gpui::test]
async fn an_answer_after_the_lane_left_its_wait_is_dropped(cx: &mut TestAppContext) {
    let slow = handler(LaneWaitAction::Keep, cx);
    slow.gated.set(true);
    let (harness, _dispatcher) = waiting_on_floor("a", cx).await;

    cx.update(|_| {
        harness
            .lanes
            .pause(&build(1), &bongos_lanes_model::Author::System)
            .unwrap();
        slow.answer_first(LaneWaitAction::Ask("too late".to_string()));
    });
    cx.run_until_parked();

    assert_eq!(harness.lane("build", 1).state, LaneState::Paused);
    assert_eq!(asked(&harness), None);
    assert!(harness.writer.read(read_asks).unwrap().is_empty());
}

// An ask left behind by a lane that left the wait is removed by the next pass and leaves the
// needs-you source.
#[gpui::test]
async fn an_ask_of_a_lane_that_stopped_is_removed(cx: &mut TestAppContext) {
    cx.update(register_needs_you);
    let _ask = handler(LaneWaitAction::Ask("mount it".to_string()), cx);
    let (harness, _dispatcher) = waiting_on_floor("a", cx).await;
    assert_eq!(cx.update(|cx| NeedsYouRegistry::collect(cx)).len(), 1);

    cx.update(|cx| {
        harness
            .lanes
            .stop(
                crate::LaneTarget::Lane(build(1)),
                bongos_lanes_model::Author::System,
                cx,
            )
            .unwrap()
    });
    cx.run_until_parked();

    assert_eq!(harness.lane("build", 1).state, LaneState::Stopped);
    assert!(harness.writer.read(read_asks).unwrap().is_empty());
    assert!(cx.update(|cx| NeedsYouRegistry::collect(cx)).is_empty());
}

/// A project profile whose `verify_lane` fails for the lanes listed.
#[derive(Default)]
pub(super) struct VerifyProfile {
    pub(super) failing: Mutex<BTreeSet<LaneId>>,
}

/// The problem a failing verify reports.
pub(super) const UNMOUNTED: &str = "/mnt/fast is not mounted";

impl LaneProfileSource for VerifyProfile {
    fn verify_lane(
        &self,
        _project: &str,
        lane: &LaneId,
        _cx: &mut App,
    ) -> Task<Result<(), Vec<Problem>>> {
        if !self.failing.lock().unwrap().contains(lane) {
            return Task::ready(Ok(()));
        }
        Task::ready(Err(vec![Problem {
            requirement: "the lane's drive is mounted".to_string(),
            message: UNMOUNTED.to_string(),
            hint: None,
        }]))
    }
}

/// Registers a profile whose `verify_lane` fails for `lanes`.
pub(super) fn failing_verify(lanes: &[LaneId], cx: &mut TestAppContext) -> Arc<VerifyProfile> {
    let profile = Arc::new(VerifyProfile::default());
    profile
        .failing
        .lock()
        .unwrap()
        .extend(lanes.iter().cloned());
    let source = profile.clone();
    cx.update(|cx| LaneProfiles::register(source, cx));
    profile
}

// a failing `verify_lane` at a job's start clears the reserved job, so a second
// lane takes a task of the same one-instance workflow; the lane waits with the problems.
#[gpui::test]
async fn a_failing_verify_clears_the_job_for_another_lane(cx: &mut TestAppContext) {
    let profile = failing_verify(&[build(1)], cx);
    let harness = harness(&builds(2), graph(vec![leaf("a"), leaf("b")]), &[("w", 1)]);
    let _started = start(&harness, start_request(), cx).await;

    assert_eq!(harness.engine.job_tasks(), ["b"]);
    let waiting = harness.lane("build", 1);
    assert_eq!(waiting.state, LaneState::Waiting);
    assert_eq!(waiting.job, None);
    assert_eq!(
        waiting.reason,
        Some(WaitReason::Environment(UNMOUNTED.to_string()))
    );
    // The owner stays, so the lane takes its task again once the drive is back.
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.state, OwnerState::Active);
    assert_eq!(owner.lane, Some(build(1)));
    assert_eq!(harness.lane("build", 2).state, LaneState::Running);

    profile.failing.lock().unwrap().clear();
    harness.update_graph(|tasks| super::finish(tasks, "b"));
    harness.set_run("b", RunStatus::Succeeded, cx);
    cx.executor().advance_clock(Duration::from_secs(60));
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["b", "a"]);
}

/// Every state event the writer sent since `events` subscribed.
fn drain(events: &mut futures::channel::mpsc::UnboundedReceiver<StateEvent>) -> Vec<StateEvent> {
    let mut drained = Vec::new();
    while let Ok(event) = events.try_recv() {
        drained.push(event);
    }
    drained
}

// a retry of a halted lane's failed run makes the lane run and its owner active,
// with the halt reason gone and the retry in the task's history, in one transaction; a second
// report of the retry changes nothing, and `lanes: resume` says it already runs.
#[gpui::test]
async fn a_retried_run_resumes_its_halted_lane_in_one_transaction(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let _started = start(&harness, start_request(), cx).await;
    let run = harness.run_of("a");
    harness.set_run("a", RunStatus::Failed, cx);
    assert_eq!(harness.lane("build", 1).state, LaneState::Halted);
    let halted_history = history_of(&harness, "a");

    let mut events = harness.writer.subscribe();
    let generation = harness.writer.generation().unwrap();
    // `workflow: retry step` makes the failed run live again.
    cx.update(|cx| harness.engine.set(&run, RunStatus::Running, cx));
    cx.run_until_parked();

    // One commit wrote the lane, the owner and the history together: no state in between was
    // ever stored.
    let written = drain(&mut events);
    assert_eq!(written.len(), 1, "{written:?}");
    let StateEvent::Changed {
        generation: committed,
        task_ids,
        namespaces,
    } = &written[0];
    assert_eq!(*committed, generation + 1);
    assert_eq!(task_ids, &["a".to_string()]);
    assert!(namespaces.iter().any(|namespace| namespace == "lanes"));

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Running);
    assert_eq!(lane.reason, None);
    assert_eq!(
        lane.job.as_ref().map(|job| job.run_id.as_str()),
        Some(run.as_str())
    );
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.state, OwnerState::Active);
    assert_eq!(owner.reason, None);
    let mut expected = halted_history;
    expected.push(format!("resumed by retry of run {}", short_run(&run)));
    assert_eq!(history_of(&harness, "a"), expected);

    // The retry reported again changes nothing.
    let resumed = harness.state();
    cx.update(|cx| harness.engine.set(&run, RunStatus::Running, cx));
    cx.run_until_parked();
    assert!(drain(&mut events).is_empty());
    assert_eq!(harness.state(), resumed);
    assert_eq!(history_of(&harness, "a"), expected);

    assert_eq!(
        harness.lanes.resume(&build(1)).unwrap(),
        "build-1 is already running"
    );
    assert_eq!(harness.state(), resumed);
}
