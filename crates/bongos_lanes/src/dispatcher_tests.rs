use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_lanes_model::{
    Author, EdgeKind, JobRecord, LaneId, LaneState, LaneTable, LanesState, OpenTodo, OwnerRecord,
    OwnerState, PickEdge, PickGraph, PickTask, Problem, RunSubject, TaskId, TaskState, WaitReason,
    parse,
};
use bongos_project_repo::StateWriter;
use bongos_workflow::lifecycle::RefusalKind;
use bongos_workflow::{Refusal, RunId, RunStatus, StartRequest, StartSubject, WorkflowEvent};
use bongos_workflow_model::record::StartedBy;
use bongos_workflow_model::{UnattendedChoice, UnattendedSource};
use futures::channel::oneshot;
use gpui::{App, TestAppContext};
use tempfile::TempDir;

use crate::dispatcher::{
    DispatcherRecord, JobCommit, JobDefinitionPolicy, JobEngine, JobSetup, Lanes, LanesHost,
    LanesParts, LanesStarted, NOTHING_READY, StartLanes, VerifiedCommitPolicy, put_dispatcher,
};
use crate::lane_control::RunEnd;
use crate::store::{put_lane, put_owner, read_state};
use crate::task_trees::LaneTreeSetup;
use crate::test_support::{SINCE, tree, writer};

const DISPATCHER_RUN: &str = "0190dispatcher";

pub(crate) struct TestHost {
    project: String,
    writer: Arc<StateWriter>,
    table: String,
    graph: RefCell<PickGraph>,
    /// The next graph snapshots that write to the store first, as another writer would.
    writes_on_graph: Cell<usize>,
    live_hosts: RefCell<BTreeSet<String>>,
    /// Job preparations wait until released.
    gated: Cell<bool>,
    gates: RefCell<Vec<oneshot::Sender<Result<JobSetup, WaitReason>>>>,
    /// Job preparations fail with this.
    setup_error: RefCell<Option<WaitReason>>,
    /// What every job's preparation offers of the lane's trees outside the job.
    pub(crate) lane_trees: RefCell<Option<LaneTreeSetup>>,
}

/// A policy that answers every job with one commit decision.
struct FixedPolicy(JobCommit);

impl JobDefinitionPolicy for FixedPolicy {
    fn commit_for_job(&self, _lane: &LaneId, _master: &str) -> JobCommit {
        self.0.clone()
    }
}

impl LanesHost for TestHost {
    fn project_id(&self) -> String {
        self.project.clone()
    }

    fn is_repository_project(&self) -> bool {
        true
    }

    fn lane_table(&self, _commit: &str) -> Result<LaneTable, String> {
        parse(&self.table).map_err(|error| error.message)
    }

    fn check_locations(
        &self,
        _table: &LaneTable,
        _lanes: &[LaneId],
        _cx: &App,
    ) -> Result<(), Vec<Problem>> {
        Ok(())
    }

    fn graph(&self, _cx: &App) -> Option<PickGraph> {
        let left = self.writes_on_graph.get();
        if left > 0 {
            self.writes_on_graph.set(left - 1);
            self.writer
                .transact(|tx| {
                    tx.kv("test")
                        .put("moved", toml::Value::Integer(left as i64))
                })
                .unwrap();
        }
        Some(self.graph.borrow().clone())
    }

    fn host_id(&self) -> String {
        "this-host".to_string()
    }

    fn host_live(&self, host: &str) -> bool {
        self.live_hosts.borrow().contains(host)
    }

    fn now(&self) -> String {
        SINCE.to_string()
    }

    fn reevaluate_interval(&self, _cx: &App) -> Duration {
        Duration::from_secs(60)
    }

    fn prepare_job(
        &self,
        _lane: &LaneId,
        _job: &JobRecord,
        cx: &mut App,
    ) -> gpui::Task<Result<JobSetup, WaitReason>> {
        if let Some(reason) = self.setup_error.borrow().clone() {
            return gpui::Task::ready(Err(reason));
        }
        if !self.gated.get() {
            return gpui::Task::ready(Ok(JobSetup {
                lane_trees: self.lane_trees.borrow().clone(),
                ..JobSetup::default()
            }));
        }
        let (sender, receiver) = oneshot::channel();
        self.gates.borrow_mut().push(sender);
        cx.foreground_executor().spawn(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(WaitReason::Other("gate dropped".to_string())))
        })
    }
}

impl TestHost {
    fn release(&self) {
        for gate in self.gates.borrow_mut().drain(..) {
            gate.send(Ok(JobSetup::default())).ok();
        }
    }
}

type Observer = Box<dyn Fn(&WorkflowEvent, &mut App)>;

/// The run engine of the tests: it records every start with the lanes state at that moment and
/// keeps each run's status, which the test changes.
pub(crate) struct FakeEngine {
    writer: Arc<StateWriter>,
    /// Every job start, in order, with the lanes state the store held when it was asked.
    jobs: RefCell<Vec<(StartRequest, LanesState)>>,
    dispatcher_starts: RefCell<Vec<StartRequest>>,
    status: RefCell<BTreeMap<String, (String, RunStatus)>>,
    limits: BTreeMap<String, u32>,
    /// The next job start is refused with this.
    refuse: RefCell<Option<Refusal>>,
    observers: RefCell<Vec<Rc<Observer>>>,
    /// Every end of the dispatcher's run.
    ends: RefCell<Vec<(String, RunEnd, String)>>,
}

impl JobEngine for FakeEngine {
    fn start(&self, request: StartRequest, _cx: &mut App) -> Result<RunId, Refusal> {
        if matches!(request.subject, StartSubject::Project { .. }) && request.run_id.is_none() {
            self.dispatcher_starts.borrow_mut().push(request);
            return Ok(DISPATCHER_RUN.to_string());
        }
        let seen = self.writer.read(read_state).unwrap();
        self.jobs.borrow_mut().push((request.clone(), seen));
        if let Some(refusal) = self.refuse.borrow_mut().take() {
            return Err(refusal);
        }
        let run = request.run_id.clone().unwrap();
        self.status
            .borrow_mut()
            .insert(run.clone(), (request.workflow.unwrap(), RunStatus::Running));
        Ok(run)
    }

    fn knows(&self, run: &str) -> bool {
        self.status.borrow().contains_key(run)
    }

    fn active_runs_of(&self, workflow: &str) -> Vec<RunId> {
        self.status
            .borrow()
            .iter()
            .filter(|(_, (name, status))| name == workflow && status.is_active())
            .map(|(run, _)| run.clone())
            .collect()
    }

    fn instance_limit(&self, workflow: &str) -> Option<u32> {
        self.limits.get(workflow).copied()
    }

    fn observe(&self, f: Observer, _cx: &mut App) {
        self.observers.borrow_mut().push(Rc::new(f));
    }

    fn end_dispatcher(&self, run: &str, end: RunEnd, note: &str, _cx: &mut App) {
        self.ends
            .borrow_mut()
            .push((run.to_string(), end, note.to_string()));
    }
}

impl FakeEngine {
    /// The run's status becomes `status`, and every observer hears it.
    fn set(&self, run: &str, status: RunStatus, cx: &mut App) {
        if let Some(entry) = self.status.borrow_mut().get_mut(run) {
            entry.1 = status;
        }
        let event = WorkflowEvent::Changed {
            run: run.to_string(),
            task_id: String::new(),
            state: status,
        };
        let observers: Vec<Rc<Observer>> = self.observers.borrow().clone();
        for observer in observers {
            observer(&event, cx);
        }
    }

    /// The task of each job start, in order.
    pub(crate) fn job_tasks(&self) -> Vec<String> {
        self.jobs
            .borrow()
            .iter()
            .map(|(request, _)| request.task_id().unwrap_or_default().to_string())
            .collect()
    }
}

pub(crate) struct Harness {
    _root: TempDir,
    pub(crate) writer: Arc<StateWriter>,
    pub(crate) host: Rc<TestHost>,
    pub(crate) engine: Rc<FakeEngine>,
    lanes: Lanes,
}

pub(crate) fn harness(table: &str, graph: PickGraph, limits: &[(&str, u32)]) -> Harness {
    harness_with(table, graph, limits, Rc::new(VerifiedCommitPolicy))
}

/// A harness of the project `project`, with its own store.
pub(crate) fn harness_in(project: &str, table: &str, graph: PickGraph) -> Harness {
    harness_of(project, table, graph, &[], Rc::new(VerifiedCommitPolicy))
}

fn harness_with(
    table: &str,
    graph: PickGraph,
    limits: &[(&str, u32)],
    policy: Rc<dyn JobDefinitionPolicy>,
) -> Harness {
    harness_of("project-1", table, graph, limits, policy)
}

fn harness_of(
    project: &str,
    table: &str,
    graph: PickGraph,
    limits: &[(&str, u32)],
    policy: Rc<dyn JobDefinitionPolicy>,
) -> Harness {
    let root = TempDir::new().unwrap();
    let writer = writer(&root);
    writer
        .transact(|tx| tx.set_verified_commit("c1", Arc::new(tree(&[]))))
        .unwrap();
    let host = Rc::new(TestHost {
        project: project.to_string(),
        writer: writer.clone(),
        table: format!("schema = 1\n{table}"),
        graph: RefCell::new(graph),
        writes_on_graph: Cell::new(0),
        live_hosts: RefCell::new(BTreeSet::new()),
        gated: Cell::new(false),
        gates: RefCell::new(Vec::new()),
        setup_error: RefCell::new(None),
        lane_trees: RefCell::new(None),
    });
    let engine = Rc::new(FakeEngine {
        writer: writer.clone(),
        jobs: RefCell::new(Vec::new()),
        dispatcher_starts: RefCell::new(Vec::new()),
        status: RefCell::new(BTreeMap::new()),
        limits: limits
            .iter()
            .map(|(name, limit)| (name.to_string(), *limit))
            .collect(),
        refuse: RefCell::new(None),
        observers: RefCell::new(Vec::new()),
        ends: RefCell::new(Vec::new()),
    });
    let lanes = Lanes::new(LanesParts {
        host: host.clone(),
        engine: engine.clone(),
        policy,
        writer: writer.clone(),
    });
    Harness {
        _root: root,
        writer,
        host,
        engine,
        lanes,
    }
}

pub(crate) fn start_request() -> StartLanes {
    StartLanes {
        bound: None,
        max_jobs: None,
        unattended: None,
        by: Author::Person("kim".to_string()),
    }
}

pub(crate) async fn start(
    harness: &Harness,
    request: StartLanes,
    cx: &mut TestAppContext,
) -> LanesStarted {
    let task = cx.update(|cx| harness.lanes.start(request, cx));
    let started = task.await.unwrap();
    cx.run_until_parked();
    started
}

impl Harness {
    pub(crate) fn state(&self) -> LanesState {
        self.writer.read(read_state).unwrap()
    }

    pub(crate) fn lane(&self, kind: &str, index: u32) -> bongos_lanes_model::LaneRecord {
        self.state().lanes[&LaneId::new(kind, index)].clone()
    }

    /// The run id of the job of the lane that works on `task`.
    fn run_of(&self, task: &str) -> String {
        self.state()
            .lanes
            .values()
            .filter_map(|lane| lane.job.as_ref())
            .find(|job| job.subject == bongos_lanes_model::RunSubject::Task(TaskId::new(task)))
            .map(|job| job.run_id.clone())
            .unwrap_or_else(|| panic!("no lane works on {task}"))
    }

    fn set_run(&self, task: &str, status: RunStatus, cx: &mut TestAppContext) {
        let run = self.run_of(task);
        cx.update(|cx| self.engine.set(&run, status, cx));
        cx.run_until_parked();
    }

    fn update_graph(&self, f: impl FnOnce(&mut Vec<PickTask>)) {
        let graph = self.host.graph.borrow().clone();
        let top = graph.top().to_vec();
        let mut tasks: Vec<PickTask> = graph.walk().cloned().collect();
        f(&mut tasks);
        *self.host.graph.borrow_mut() = PickGraph::new(top, tasks);
    }
}

/// A ready leaf whose open todo runs workflow `w`.
pub(crate) fn leaf(id: &str) -> PickTask {
    let mut task = PickTask::leaf(id);
    task.open_todo = Some(OpenTodo {
        index: 0,
        name: "implement".to_string(),
        workflow: Some("w".to_string()),
    });
    task
}

fn tagged(id: &str, tags: &[&str]) -> PickTask {
    let mut task = leaf(id);
    task.areas = tags.iter().map(|tag| tag.to_string()).collect();
    task
}

fn after(mut task: PickTask, target: &str) -> PickTask {
    task.edges.push(PickEdge {
        kind: EdgeKind::After,
        target: TaskId::new(target),
        unblock: "merged".to_string(),
        reached: false,
        progress: "in-progress; needs merged".to_string(),
    });
    task
}

fn graph(tasks: Vec<PickTask>) -> PickGraph {
    let top = tasks.iter().map(|task| task.id.clone()).collect();
    PickGraph::new(top, tasks)
}

pub(crate) fn finish(tasks: &mut [PickTask], id: &str) {
    for task in tasks.iter_mut() {
        if task.id.0 == id {
            task.state = TaskState::Done;
            task.open_todo = None;
        }
        for edge in &mut task.edges {
            if edge.target.0 == id {
                edge.reached = true;
            }
        }
    }
}

pub(crate) fn builds(count: u32) -> String {
    format!("[[kind]]\nname = \"build\"\ncount = {count}\n")
}

#[gpui::test]
async fn first_start_creates_every_lane_idle_with_its_reason(cx: &mut TestAppContext) {
    let mut held = leaf("a");
    held.held = Some("restore in progress".to_string());
    let table = format!("{}[[kind]]\nname = \"docs\"\ncount = 1\n", builds(2));
    let harness = harness(&table, graph(vec![held]), &[]);

    let started = start(&harness, start_request(), cx).await;

    assert_eq!(started.run_id, DISPATCHER_RUN);
    assert!(!started.attached);
    let dispatcher_starts = harness.engine.dispatcher_starts.borrow();
    assert_eq!(dispatcher_starts.len(), 1);
    assert_eq!(
        dispatcher_starts[0].subject,
        StartSubject::Project {
            id: "project-1".to_string()
        }
    );
    assert_eq!(dispatcher_starts[0].workflow.as_deref(), Some("lanes"));
    let state = harness.state();
    let ids: Vec<String> = state.lanes.keys().map(ToString::to_string).collect();
    assert_eq!(ids, ["build-1", "build-2", "docs-1"]);
    for lane in state.lanes.values() {
        assert_eq!(lane.state, LaneState::Idle, "{}", lane.id());
        assert_eq!(
            lane.reason,
            Some(WaitReason::Held {
                reason: "restore in progress".to_string()
            }),
            "{}",
            lane.id()
        );
    }
    assert!(harness.engine.jobs.borrow().is_empty());
    let record: DispatcherRecord = harness
        .writer
        .transact(crate::dispatcher::dispatcher_record)
        .unwrap()
        .unwrap();
    assert_eq!(record.run_id, DISPATCHER_RUN);
    assert_eq!(record.host, "this-host");
}

#[gpui::test]
async fn start_attaches_to_the_dispatcher_of_a_live_host(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    harness
        .host
        .live_hosts
        .borrow_mut()
        .insert("other-host".to_string());
    harness
        .writer
        .transact(|tx| {
            put_dispatcher(
                tx,
                &DispatcherRecord {
                    run_id: "0190running".to_string(),
                    host: "other-host".to_string(),
                    started: SINCE.to_string(),
                    by: Author::System,
                    bound: None,
                    unattended: None,
                    definition_commit: None,
                    stopped_by: None,
                    ended: None,
                },
            )
        })
        .unwrap();

    let started = start(&harness, start_request(), cx).await;

    assert!(started.attached);
    assert_eq!(started.run_id, "0190running");
    assert!(started.dispatcher.is_none());
    assert!(harness.engine.dispatcher_starts.borrow().is_empty());
    assert!(harness.state().lanes.is_empty());
}

// PRD 6: three lanes busy at once.
#[gpui::test]
async fn three_lanes_take_three_tasks_at_once(cx: &mut TestAppContext) {
    let harness = harness(
        &builds(3),
        graph(vec![leaf("a"), leaf("b"), leaf("c")]),
        &[],
    );

    let started = start(&harness, start_request(), cx).await;
    let _dispatcher = started.dispatcher.unwrap();

    assert_eq!(harness.engine.job_tasks(), ["a", "b", "c"]);
    let state = harness.state();
    for (index, task) in [(1, "a"), (2, "b"), (3, "c")] {
        let id = LaneId::new("build", index);
        let lane = &state.lanes[&id];
        assert_eq!(lane.state, LaneState::Running);
        let owner = &state.owners[&TaskId::new(task)];
        assert_eq!(owner.state, OwnerState::Active);
        assert_eq!(owner.lane.as_ref(), Some(&id));
    }
    for (request, _) in harness.engine.jobs.borrow().iter() {
        let lane = request.lane.as_ref().unwrap();
        let record = &state.lanes[&LaneId::parse(&lane.id).unwrap()];
        assert_eq!(
            request.run_id,
            record.job.as_ref().map(|job| job.run_id.clone())
        );
        assert_eq!(request.started_by, StartedBy::Lane);
        assert_eq!(request.parent.as_deref(), Some(DISPATCHER_RUN));
        assert_eq!(request.workflow.as_deref(), Some("w"));
        assert!(matches!(
            request.subject,
            StartSubject::Task {
                todo_index: Some(0),
                ..
            }
        ));
    }
}

// The owner, the lane and the reserved run id are committed before the run is asked for; with
// the start never reached, the job has no run.
#[gpui::test]
async fn the_records_commit_before_the_run_starts(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    harness.host.gated.set(true);

    let _started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Running);
    let run = lane.job.as_ref().unwrap().run_id.clone();
    assert!(harness.engine.jobs.borrow().is_empty());
    assert!(!harness.engine.knows(&run));
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].state,
        OwnerState::Active
    );

    harness.host.release();
    cx.run_until_parked();

    let jobs = harness.engine.jobs.borrow();
    assert_eq!(jobs.len(), 1);
    let (request, seen) = &jobs[0];
    assert_eq!(request.run_id.as_deref(), Some(run.as_str()));
    let seen_lane = &seen.lanes[&LaneId::new("build", 1)];
    assert_eq!(seen_lane.job.as_ref().map(|job| &job.run_id), Some(&run));
    assert_eq!(seen.owners[&TaskId::new("a")].state, OwnerState::Active);
}

// A reserved job that has not started counts toward its workflow's instances.
#[gpui::test]
async fn a_reserved_job_counts_toward_instances(cx: &mut TestAppContext) {
    let harness = harness(&builds(2), graph(vec![leaf("a"), leaf("b")]), &[("w", 1)]);
    harness.host.gated.set(true);

    let _started = start(&harness, start_request(), cx).await;

    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    let second = harness.lane("build", 2);
    assert_eq!(second.state, LaneState::Idle);
    assert!(
        matches!(
            second.reason,
            Some(WaitReason::InstanceLimit { limit: 1, .. })
        ),
        "{:?}",
        second.reason
    );

    harness.host.release();
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert!(matches!(
        harness.lane("build", 2).reason,
        Some(WaitReason::InstanceLimit { active: 1, .. })
    ));

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);
    harness.host.release();
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
}

fn freeing_at_once(exempt: bool) -> Harness {
    let mut table = builds(2);
    if exempt {
        table.push_str("[[exempt]]\ntags = [\"db\"]\n");
    }
    harness(
        &table,
        graph(vec![
            leaf("p"),
            leaf("q"),
            tagged("t1", &["db"]),
            tagged("t2", &["db"]),
        ]),
        &[],
    )
}

/// Both lanes' jobs end in one tick.
fn end_both(harness: &Harness, cx: &mut TestAppContext) {
    harness.update_graph(|tasks| {
        finish(tasks, "p");
        finish(tasks, "q");
    });
    let p = harness.run_of("p");
    let q = harness.run_of("q");
    cx.update(|cx| {
        harness.engine.set(&p, RunStatus::Succeeded, cx);
        harness.engine.set(&q, RunStatus::Succeeded, cx);
    });
    cx.run_until_parked();
}

// PRD 14: two lanes freeing in one tick with two tasks sharing a tag take one.
#[gpui::test]
async fn two_lanes_freeing_at_once_take_one_of_two_tasks_sharing_a_tag(cx: &mut TestAppContext) {
    let harness = freeing_at_once(false);
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.engine.job_tasks(), ["p", "q"]);

    end_both(&harness, cx);

    assert_eq!(harness.engine.job_tasks(), ["p", "q", "t1"]);
    let state = harness.state();
    // Both owners turned done and, with no checks owed under them, were cleared.
    for task in ["p", "q"] {
        assert!(!state.owners.contains_key(&TaskId::new(task)), "{task}");
        assert_eq!(history_of(&harness, task), [crate::CLEARED_NO_CHECKS]);
    }
    let running: Vec<_> = state
        .lanes
        .values()
        .filter(|lane| lane.state == LaneState::Running)
        .collect();
    assert_eq!(running.len(), 1);
    let idle: Vec<_> = state
        .lanes
        .values()
        .filter(|lane| lane.state == LaneState::Idle)
        .collect();
    assert_eq!(idle.len(), 1);
    assert!(
        matches!(&idle[0].reason, Some(WaitReason::Area { tag, .. }) if tag == "db"),
        "{:?}",
        idle[0].reason
    );
}

// PRD 14: with an exemption for the tag, both lanes take one.
#[gpui::test]
async fn two_lanes_freeing_at_once_take_both_tasks_with_an_exemption(cx: &mut TestAppContext) {
    let harness = freeing_at_once(true);
    let _started = start(&harness, start_request(), cx).await;

    end_both(&harness, cx);

    assert_eq!(harness.engine.job_tasks(), ["p", "q", "t1", "t2"]);
    for lane in harness.state().lanes.values() {
        assert_eq!(lane.state, LaneState::Running, "{}", lane.id());
    }
}

// PRD 26: a failed job halts its lane and task, the other lanes go on, and the halted task
// blocks its area; a retry makes the lane run again.
#[gpui::test]
async fn a_failed_job_halts_its_lane_and_task_and_others_go_on(cx: &mut TestAppContext) {
    let harness = harness(
        &builds(2),
        graph(vec![
            tagged("a", &["x"]),
            tagged("c", &["y"]),
            tagged("d", &["x"]),
        ]),
        &[],
    );
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.engine.job_tasks(), ["a", "c"]);
    let run_a = harness.run_of("a");

    harness.set_run("a", RunStatus::Failed, cx);

    let halted = harness.lane("build", 1);
    assert_eq!(halted.state, LaneState::Halted);
    assert_eq!(
        halted.reason,
        Some(WaitReason::Failed(format!(
            "run {} failed",
            bongos_lanes_model::short_run(&run_a)
        )))
    );
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.state, OwnerState::Halted);
    assert_eq!(harness.lane("build", 2).state, LaneState::Running);

    harness.update_graph(|tasks| finish(tasks, "c"));
    harness.set_run("c", RunStatus::Succeeded, cx);

    assert_eq!(harness.engine.job_tasks(), ["a", "c"]);
    let free = harness.lane("build", 2);
    assert_eq!(free.state, LaneState::Idle);
    assert!(
        matches!(&free.reason, Some(WaitReason::Area { tag, with, .. }) if tag == "x" && with.0 == "a"),
        "{:?}",
        free.reason
    );
    assert_eq!(harness.lane("build", 1).state, LaneState::Halted);

    cx.update(|cx| harness.engine.set(&run_a, RunStatus::Running, cx));
    cx.run_until_parked();

    let resumed = harness.lane("build", 1);
    assert_eq!(resumed.state, LaneState::Running);
    assert_eq!(resumed.reason, None);
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.state, OwnerState::Active);
    assert_eq!(owner.reason, None);
    let history = harness
        .writer
        .read(|read| read.task("a"))
        .unwrap()
        .unwrap()
        .history;
    let events: Vec<String> = history.into_iter().map(|entry| entry.event).collect();
    let short = bongos_lanes_model::short_run(&run_a);
    assert_eq!(
        events,
        [
            format!("run {short} halted"),
            format!("resumed by retry of run {short}")
        ]
    );
}

// PRD 31: the unattended choice and its source; with nothing set it is the project's, true.
#[gpui::test]
async fn a_job_records_the_unattended_choice_and_its_source(cx: &mut TestAppContext) {
    let unattended = |harness: &Harness| {
        harness.engine.jobs.borrow()[0]
            .0
            .unattended
            .expect("a lane job always records the choice")
    };

    let plain = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let _started = start(&plain, start_request(), cx).await;
    assert_eq!(
        unattended(&plain),
        UnattendedChoice {
            value: true,
            source: UnattendedSource::Project
        }
    );
    assert_eq!(
        plain.engine.dispatcher_starts.borrow()[0].unattended,
        Some(UnattendedChoice {
            value: true,
            source: UnattendedSource::Project
        })
    );

    let kind = harness(
        &format!("{}unattended = false\n", builds(1)),
        graph(vec![leaf("a")]),
        &[],
    );
    let _started = start(&kind, start_request(), cx).await;
    assert_eq!(
        unattended(&kind),
        UnattendedChoice {
            value: false,
            source: UnattendedSource::Lane
        }
    );

    let chosen = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let request = StartLanes {
        unattended: Some(false),
        ..start_request()
    };
    let _started = start(&chosen, request, cx).await;
    assert_eq!(
        unattended(&chosen),
        UnattendedChoice {
            value: false,
            source: UnattendedSource::Start
        }
    );
}

// A task depending on an unfinished task is never picked; finishing it makes it the next pick.
#[gpui::test]
async fn a_task_waiting_on_an_unfinished_task_is_picked_once_it_finishes(cx: &mut TestAppContext) {
    let harness = harness(
        &builds(2),
        graph(vec![leaf("a"), after(leaf("b"), "a")]),
        &[],
    );
    let _started = start(&harness, start_request(), cx).await;

    assert_eq!(harness.engine.job_tasks(), ["a"]);
    let waiting = harness.lane("build", 2);
    assert_eq!(waiting.state, LaneState::Idle);
    assert!(
        matches!(&waiting.reason, Some(WaitReason::Dependency { on, .. }) if on.0 == "a"),
        "{:?}",
        waiting.reason
    );

    harness.update_graph(|tasks| finish(tasks, "a"));
    harness.set_run("a", RunStatus::Succeeded, cx);

    assert_eq!(harness.engine.job_tasks(), ["a", "b"]);
}

// A pass evaluates a lane again when the store moved between its snapshot and its transaction.
#[gpui::test]
async fn a_pass_restarts_when_the_store_moved(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    harness.host.writes_on_graph.set(1);

    let started = start(&harness, start_request(), cx).await;

    let dispatcher = started.dispatcher.unwrap();
    assert_eq!(dispatcher.restarts(), 1);
    assert_eq!(harness.engine.job_tasks(), ["a"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
}

// A refusal of the machine leaves the lane waiting, keeping its task, and nothing retries it
// until a timed pass.
#[gpui::test]
async fn an_environment_refusal_leaves_the_lane_waiting(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    *harness.engine.refuse.borrow_mut() = Some(Refusal::new(
        RefusalKind::LowDiskSpace,
        "Only 1 GB is free.",
        "Free some space.",
    ));

    let started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.job, None);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Environment(
            "Only 1 GB is free. Free some space.".to_string()
        ))
    );
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].state,
        OwnerState::Active
    );
    assert_eq!(harness.engine.job_tasks(), ["a"]);

    cx.update(|cx| started.dispatcher.as_ref().unwrap().pick_now(cx));
    cx.run_until_parked();
    assert_eq!(harness.engine.job_tasks(), ["a", "a"]);
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
}

// Any other refusal halts the lane and its task with the refusal's text.
#[gpui::test]
async fn another_refusal_halts_the_lane_and_its_task(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    *harness.engine.refuse.borrow_mut() = Some(Refusal::new(
        RefusalKind::Invalid,
        "Workflow w is not valid.",
        "",
    ));

    let _started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Halted);
    assert_eq!(lane.job, None);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Failed("Workflow w is not valid.".to_string()))
    );
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.state, OwnerState::Halted);
    assert_eq!(owner.reason.as_deref(), Some("Workflow w is not valid."));
}

// A run waiting on the person keeps its lane running; a cancel pauses the lane and the owner
// keeps the task.
#[gpui::test]
async fn waiting_on_you_keeps_the_lane_and_a_cancel_pauses_it(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let _started = start(&harness, start_request(), cx).await;
    let run = harness.run_of("a");

    harness.set_run("a", RunStatus::WaitingOnYou, cx);
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Running);
    assert_eq!(lane.job.as_ref().map(|job| job.run_id.clone()), Some(run));

    harness.set_run("a", RunStatus::Cancelled, cx);
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Paused);
    assert_eq!(lane.job, None);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Other("job cancelled".to_string()))
    );
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.state, OwnerState::Active);
    assert_eq!(owner.lane, Some(LaneId::new("build", 1)));
    assert_eq!(harness.engine.job_tasks(), ["a"]);
}

pub(crate) fn history_of(harness: &Harness, task: &str) -> Vec<String> {
    harness
        .writer
        .read(|read| read.task(task))
        .unwrap()
        .unwrap()
        .history
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

// A rollout that says wait leaves the lane idle with its reason and picks nothing.
#[gpui::test]
async fn a_policy_that_waits_leaves_the_lane_idle_with_its_reason(cx: &mut TestAppContext) {
    let harness = harness_with(
        &builds(1),
        graph(vec![leaf("a")]),
        &[],
        Rc::new(FixedPolicy(JobCommit::Wait("rolling out".to_string()))),
    );
    let _started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(lane.job, None);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Other("rolling out".to_string()))
    );
    assert!(harness.engine.job_tasks().is_empty());
    assert!(!harness.state().owners.contains_key(&TaskId::new("a")));
}

// A rollout that says stop first stops the lane at once, since it holds no job.
#[gpui::test]
async fn a_policy_that_stops_first_stops_the_idle_lane(cx: &mut TestAppContext) {
    let harness = harness_with(
        &builds(1),
        graph(vec![leaf("a")]),
        &[],
        Rc::new(FixedPolicy(JobCommit::StopFirst("rollout".to_string()))),
    );
    let _started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Stopped);
    assert_eq!(lane.job, None);
    assert_eq!(lane.stop_requested, None);
    assert!(harness.engine.job_tasks().is_empty());
}

// A job whose preparation fails leaves the lane waiting with that reason and starts no run.
#[gpui::test]
async fn a_failed_preparation_leaves_the_lane_waiting(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    *harness.host.setup_error.borrow_mut() = Some(WaitReason::Other("no tree".to_string()));

    let _started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Waiting);
    assert_eq!(lane.job, None);
    assert_eq!(lane.reason, Some(WaitReason::Other("no tree".to_string())));
    assert!(harness.engine.job_tasks().is_empty());
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].state,
        OwnerState::Active
    );
}

// With nothing in the graph an idle lane says nothing is ready.
#[gpui::test]
async fn an_empty_graph_leaves_the_lane_idle_with_nothing_ready(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let _started = start(&harness, start_request(), cx).await;

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Idle);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Other(NOTHING_READY.to_string()))
    );
}

// A workflow that halts halts its lane and its task.
#[gpui::test]
async fn a_halted_run_halts_its_lane_and_task(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let _started = start(&harness, start_request(), cx).await;
    let run = harness.run_of("a");

    harness.set_run("a", RunStatus::Halted, cx);

    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Halted);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Failed(format!(
            "run {} halted",
            bongos_lanes_model::short_run(&run)
        )))
    );
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].state,
        OwnerState::Halted
    );
}

// A second end for a run whose end was applied changes nothing, and a success counts once.
#[gpui::test]
async fn a_repeated_end_changes_nothing(cx: &mut TestAppContext) {
    let harness = harness(&builds(2), graph(vec![leaf("a"), leaf("c")]), &[]);
    let _started = start(&harness, start_request(), cx).await;
    let run_a = harness.run_of("a");
    let run_c = harness.run_of("c");

    cx.update(|cx| harness.engine.set(&run_a, RunStatus::Failed, cx));
    cx.run_until_parked();
    let halted = harness.state();
    let events = history_of(&harness, "a");
    assert_eq!(events.len(), 1);

    cx.update(|cx| harness.engine.set(&run_a, RunStatus::Halted, cx));
    cx.run_until_parked();
    cx.update(|cx| harness.engine.set(&run_a, RunStatus::Succeeded, cx));
    cx.run_until_parked();
    assert_eq!(harness.state(), halted);
    assert_eq!(history_of(&harness, "a"), events);

    harness.update_graph(|tasks| finish(tasks, "c"));
    cx.update(|cx| harness.engine.set(&run_c, RunStatus::Succeeded, cx));
    cx.run_until_parked();
    assert_eq!(harness.lane("build", 2).jobs_done, 1);
    let ended = harness.state();

    cx.update(|cx| harness.engine.set(&run_c, RunStatus::Succeeded, cx));
    cx.run_until_parked();
    cx.update(|cx| harness.engine.set(&run_c, RunStatus::Failed, cx));
    cx.run_until_parked();
    assert_eq!(harness.state(), ended);
    assert_eq!(harness.lane("build", 2).jobs_done, 1);
}

// A task-less job failing halts its lane and leaves an unrelated owner on that lane alone.
#[gpui::test]
async fn a_failed_job_with_no_task_leaves_an_unrelated_owner_alone(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let _started = start(&harness, start_request(), cx).await;
    let lane_id = LaneId::new("build", 1);
    let owner = OwnerRecord {
        lane: Some(lane_id),
        state: OwnerState::Ahead,
        since: SINCE.to_string(),
        by: Author::System,
        reason: None,
        prepared_commit: None,
        refreshed: false,
    };
    let mut lane = harness.lane("build", 1);
    harness
        .writer
        .transact(|tx| {
            lane.state = LaneState::Running;
            lane.job = Some(JobRecord {
                run_id: "0190extension".to_string(),
                subject: RunSubject::Other("project-1".to_string()),
                workflow: "w".to_string(),
                started: SINCE.to_string(),
                definition_commit: None,
            });
            put_lane(tx, &lane)?;
            put_owner(tx, &TaskId::new("z"), &owner)
        })
        .unwrap();

    cx.update(|cx| harness.engine.set("0190extension", RunStatus::Failed, cx));
    cx.run_until_parked();

    assert_eq!(harness.lane("build", 1).state, LaneState::Halted);
    assert_eq!(harness.state().owners[&TaskId::new("z")], owner);
}

// A cancel of a failed job, whose lane is halted and still holds it, pauses the lane and clears
// the job; the owner keeps the task.
#[gpui::test]
async fn a_cancel_after_a_failure_pauses_the_halted_lane(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let _started = start(&harness, start_request(), cx).await;

    harness.set_run("a", RunStatus::Failed, cx);
    assert_eq!(harness.lane("build", 1).state, LaneState::Halted);

    harness.set_run("a", RunStatus::Cancelled, cx);
    let lane = harness.lane("build", 1);
    assert_eq!(lane.state, LaneState::Paused);
    assert_eq!(lane.job, None);
    assert_eq!(
        lane.reason,
        Some(WaitReason::Other("job cancelled".to_string()))
    );
    let owner = harness.state().owners[&TaskId::new("a")].clone();
    assert_eq!(owner.lane, Some(LaneId::new("build", 1)));
}

// A second failure of a run whose failure halted its lane changes nothing.
#[gpui::test]
async fn a_duplicate_failure_of_a_halted_lane_changes_nothing(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    let _started = start(&harness, start_request(), cx).await;

    harness.set_run("a", RunStatus::Failed, cx);
    let halted = harness.state();
    let events = history_of(&harness, "a");

    harness.set_run("a", RunStatus::Failed, cx);
    assert_eq!(harness.state(), halted);
    assert_eq!(history_of(&harness, "a"), events);
}

// A task-less job that fails halts its lane; a retry resumes it and its success leaves it idle.
#[gpui::test]
async fn a_task_less_job_resumes_on_retry_and_ends(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![]), &[]);
    let _started = start(&harness, start_request(), cx).await;
    let mut lane = harness.lane("build", 1);
    let done_before = lane.jobs_done;
    harness
        .writer
        .transact(|tx| {
            lane.state = LaneState::Running;
            lane.job = Some(JobRecord {
                run_id: "0190extension".to_string(),
                subject: RunSubject::Other("project-1".to_string()),
                workflow: "w".to_string(),
                started: SINCE.to_string(),
                definition_commit: None,
            });
            put_lane(tx, &lane)
        })
        .unwrap();

    cx.update(|cx| harness.engine.set("0190extension", RunStatus::Failed, cx));
    cx.run_until_parked();
    assert_eq!(harness.lane("build", 1).state, LaneState::Halted);

    cx.update(|cx| harness.engine.set("0190extension", RunStatus::Running, cx));
    cx.run_until_parked();
    let resumed = harness.lane("build", 1);
    assert_eq!(resumed.state, LaneState::Running);
    assert_eq!(resumed.reason, None);

    cx.update(|cx| {
        harness
            .engine
            .set("0190extension", RunStatus::Succeeded, cx)
    });
    cx.run_until_parked();
    let ended = harness.lane("build", 1);
    assert_eq!(ended.state, LaneState::Idle);
    assert_eq!(ended.job, None);
    assert_eq!(ended.jobs_done, done_before + 1);
    assert!(harness.state().owners.is_empty());
}

// A start whose lane lost its job while the job was prepared starts no run.
#[gpui::test]
async fn a_start_is_skipped_when_the_lane_no_longer_holds_the_run(cx: &mut TestAppContext) {
    let harness = harness(&builds(1), graph(vec![leaf("a")]), &[]);
    harness.host.gated.set(true);
    let _started = start(&harness, start_request(), cx).await;
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);

    let mut lane = harness.lane("build", 1);
    harness
        .writer
        .transact(|tx| {
            lane.state = LaneState::Idle;
            lane.job = None;
            put_lane(tx, &lane)
        })
        .unwrap();
    harness.host.release();
    cx.run_until_parked();

    assert!(harness.engine.job_tasks().is_empty());
}

#[path = "lane_control_tests.rs"]
mod lane_control_tests;
