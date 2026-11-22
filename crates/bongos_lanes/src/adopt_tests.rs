//! `lanes: adopt` over the fake engine of the dispatcher tests. The adopts of halted jobs run with
//! a host whose jobs run in real lane trees of a temporary git repository, so the save before the
//! cancel, the cancel and the next job starts are seen on disk.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_lanes_model::{
    Author, JobRecord, LaneId, LaneState, LaneTable, OwnerState, PickGraph, PickTask, Problem,
    TaskId, WaitReason, parse, short_run,
};
use bongos_project_repo::StateWriter;
use bongos_workflow::worktrees::run_branch_name;
use bongos_workflow::{RunStatus, RunTree};
use gpui::{App, TestAppContext};
use tempfile::TempDir;

use super::{FakeEngine, builds, fake_engine, graph, harness, leaf, start_request};
use crate::adopt::{AdoptRequest, Adopted, LeftWork};
use crate::dispatcher::{JobSetup, Lanes, LanesHost, LanesParts, VerifiedCommitPolicy};
use crate::job_trees::{JobRepository, JobStart, forbidden_for, start_job_trees};
use crate::lane_tree::{Forbidden, LastEnd, LastRun, ResetRequest};
use crate::lane_tree_tests::{Fixture, TABLE, TASK_BRANCH, git};
use crate::locations::LaneLocations;
use crate::store::{put_lane, put_owner, read_state};
use crate::task_trees::SecondTreeLeases;
use crate::test_support::{SINCE, tree, writer};
use crate::{Dispatcher, LanesStarted};

fn kim() -> Author {
    Author::Person("kim".to_string())
}

fn build(index: u32) -> LaneId {
    LaneId::new("build", index)
}

fn request(task: &str, lane: u32, reason: &str) -> AdoptRequest {
    AdoptRequest {
        task: TaskId::new(task),
        lane: build(lane),
        reason: reason.to_string(),
        by: kim(),
        confirm_drop: false,
    }
}

/// A lane host whose jobs run in the lane trees of a [`Fixture`]: each job start resets the lane's
/// tree as the dispatcher's host does, with the run that last used it.
struct TreeHost {
    engine: Rc<FakeEngine>,
    table: String,
    tasks: RefCell<Vec<PickTask>>,
    fixture: Fixture,
    leases: SecondTreeLeases,
    /// The run that last used each lane's tree.
    last: RefCell<BTreeMap<LaneId, String>>,
    /// The history entries of the saves the job starts made.
    start_saves: RefCell<Vec<String>>,
}

impl TreeHost {
    fn locations(&self, lane: &LaneId) -> LaneLocations {
        self.fixture.all[(lane.index - 1) as usize].clone()
    }

    fn forbidden(&self, lane: &LaneId) -> Forbidden {
        Forbidden {
            task_worktrees: vec![self.fixture.task_worktree.clone()],
            ..forbidden_for(
                lane,
                &self.fixture.inputs,
                &self.fixture.all,
                &bongos_work_model::store::WorkTree::default(),
            )
        }
    }

    fn tip(&self, index: u32) -> PathBuf {
        self.locations(&build(index))
            .get("tree:app")
            .unwrap()
            .path
            .clone()
    }

    /// How the engine says `run` ended.
    fn end_of(&self, run: &str) -> LastEnd {
        match self
            .engine
            .status
            .borrow()
            .get(run)
            .map(|(_, status)| *status)
        {
            Some(RunStatus::Succeeded) => LastEnd::Succeeded,
            Some(RunStatus::Cancelled) => LastEnd::Cancelled,
            Some(RunStatus::Halted) => LastEnd::Halted,
            _ => LastEnd::Failed,
        }
    }

    fn last_run(&self, run: &str) -> LastRun {
        LastRun {
            run_id: run.to_string(),
            run_branch: run_branch_name(TASK_BRANCH, run, 1),
            end: self.end_of(run),
        }
    }
}

impl LanesHost for TreeHost {
    fn project_id(&self) -> String {
        "project-1".to_string()
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

    /// The tasks, each with the run of its leaf the engine still keeps active.
    fn graph(&self, _cx: &App) -> Option<PickGraph> {
        let status = self.engine.status.borrow();
        let jobs = self.engine.jobs.borrow();
        let tasks = self
            .tasks
            .borrow()
            .iter()
            .cloned()
            .map(|mut task| {
                task.active_run = jobs
                    .iter()
                    .filter(|(request, _)| request.task_id() == Some(task.id.0.as_str()))
                    .filter_map(|(request, _)| request.run_id.clone())
                    .find(|run| status.get(run).is_some_and(|(_, state)| state.is_active()));
                task
            })
            .collect();
        Some(graph(tasks))
    }

    fn host_id(&self) -> String {
        "this-host".to_string()
    }

    fn host_live(&self, _host: &str) -> bool {
        false
    }

    fn now(&self) -> String {
        SINCE.to_string()
    }

    fn reevaluate_interval(&self, _cx: &App) -> Duration {
        Duration::from_secs(60)
    }

    fn prepare_job(
        &self,
        lane: &LaneId,
        job: &JobRecord,
        _cx: &mut App,
    ) -> gpui::Task<Result<JobSetup, WaitReason>> {
        let previous = self
            .last
            .borrow_mut()
            .insert(lane.clone(), job.run_id.clone());
        let start = JobStart {
            locations: self.locations(lane),
            table_lanes: vec![build(1), build(2)],
            primary: JobRepository::for_task(
                "app",
                self.fixture.app.clone(),
                TASK_BRANCH,
                &job.run_id,
                1,
                previous.map(|run| self.last_run(&run)),
            ),
            extra: Vec::new(),
            save_max_file_bytes: 1000,
            forbidden: self.forbidden(lane),
        };
        let fresh = async |_: &JobRepository| -> Result<PathBuf, String> {
            Err("no fresh trees here".to_string())
        };
        // Git runs to its end here: a test may block its thread.
        let started = futures::executor::block_on(start_job_trees(
            &self.fixture.git,
            &start,
            &self.leases,
            &fresh,
        ));
        if let Ok(started) = &started {
            self.start_saves
                .borrow_mut()
                .extend(started.saved.iter().map(|(_, saved)| saved.left_on()));
        }
        gpui::Task::ready(
            started
                .map(|started| JobSetup::in_lane_tree(started.trees, Vec::new(), BTreeMap::new())),
        )
    }

    fn left_work(&self, lane: &LaneId, job: &JobRecord, _cx: &App) -> Vec<LeftWork> {
        vec![LeftWork {
            git: self.fixture.git.clone(),
            request: ResetRequest {
                locations: self.locations(lane),
                repo: "app".to_string(),
                repository: self.fixture.app.clone(),
                second: false,
                add_from: TASK_BRANCH.to_string(),
                start: String::new(),
                run_branch: String::new(),
                last: Some(self.last_run(&job.run_id)),
                save_max_file_bytes: 1000,
                forbidden: self.forbidden(lane),
            },
        }]
    }
}

/// Lanes over a [`TreeHost`]; the action tests drive `lanes: adopt` over it too.
pub(crate) struct TreeHarness {
    _root: TempDir,
    writer: Arc<StateWriter>,
    host: Rc<TreeHost>,
    engine: Rc<FakeEngine>,
    pub(crate) lanes: Lanes,
    started: Option<LanesStarted>,
}

impl TreeHarness {
    /// The working tree of `build-<index>`.
    pub(crate) fn lane_tip(&self, index: u32) -> PathBuf {
        self.host.tip(index)
    }

    /// The runs the engine cancelled.
    pub(crate) fn cancelled(&self) -> Vec<String> {
        self.engine.cancelled.borrow().clone()
    }

    pub(crate) fn new() -> Self {
        let root = TempDir::new().unwrap();
        let writer = writer(&root);
        writer
            .transact(|tx| tx.set_verified_commit("c1", Arc::new(tree(&[]))))
            .unwrap();
        let engine = fake_engine(&writer, &[]);
        let host = Rc::new(TreeHost {
            engine: engine.clone(),
            table: TABLE.to_string(),
            tasks: RefCell::new(vec![leaf("a")]),
            fixture: Fixture::new(),
            leases: SecondTreeLeases::default(),
            last: RefCell::new(BTreeMap::new()),
            start_saves: RefCell::new(Vec::new()),
        });
        let lanes = Lanes::new(LanesParts {
            host: host.clone(),
            engine: engine.clone(),
            policy: Rc::new(VerifiedCommitPolicy),
            writer: writer.clone(),
        });
        Self {
            _root: root,
            writer,
            host,
            engine,
            lanes,
            started: None,
        }
    }

    fn dispatcher(&self) -> &Dispatcher {
        self.started
            .as_ref()
            .and_then(|started| started.dispatcher.as_ref())
            .unwrap()
    }

    pub(crate) fn state(&self) -> bongos_lanes_model::LanesState {
        self.writer.read(read_state).unwrap()
    }

    pub(crate) fn history(&self, task: &str) -> Vec<String> {
        self.writer
            .read(|read| read.task(task))
            .unwrap()
            .map(|record| {
                record
                    .history
                    .into_iter()
                    .map(|entry| entry.event)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn job_run(&self, lane: u32) -> String {
        self.state().lanes[&build(lane)]
            .job
            .as_ref()
            .unwrap()
            .run_id
            .clone()
    }

    fn set(&self, run: &str, status: RunStatus, cx: &mut TestAppContext) {
        cx.update(|cx| self.engine.set(run, status, cx));
        cx.run_until_parked();
    }

    /// Starts the lanes: `build-1` takes `a` in its tree.
    async fn start(&mut self, cx: &mut TestAppContext) {
        let starting = cx.update(|cx| self.lanes.start(start_request(), cx));
        self.started = Some(starting.await.unwrap());
        cx.run_until_parked();
        assert_eq!(self.state().lanes[&build(1)].state, LaneState::Running);
    }

    /// `build-1` ran `a` in its tree, left work there and failed; returns the run.
    pub(crate) async fn halted(&mut self, cx: &mut TestAppContext, leave_work: bool) -> String {
        self.start(cx).await;
        let run = self.job_run(1);
        let tip = self.host.tip(1);
        assert_eq!(
            git(&tip, &["branch", "--show-current"]),
            run_branch_name(TASK_BRANCH, &run, 1)
        );
        if leave_work {
            std::fs::write(tip.join("work.txt"), "half done\n").unwrap();
            std::fs::write(tip.join("README.md"), "edited\n").unwrap();
            std::fs::write(tip.join("big.bin"), vec![b'x'; 2000]).unwrap();
        }
        self.set(&run, RunStatus::Failed, cx);
        let state = self.state();
        assert_eq!(state.lanes[&build(1)].state, LaneState::Halted);
        assert_eq!(state.owners[&TaskId::new("a")].state, OwnerState::Halted);
        run
    }

    async fn adopt(
        &self,
        request: AdoptRequest,
        cx: &mut TestAppContext,
    ) -> anyhow::Result<Adopted> {
        let adopting = cx.update(|cx| self.lanes.adopt(request, cx));
        let result = adopting.await;
        cx.run_until_parked();
        result
    }
}

fn subject(app: &Path, commit: &str) -> String {
    git(app, &["log", "-1", "--format=%s", commit])
}

// Adopting a halted task saves the work its failed run left, then cancels that run with
// the history line, moves the owner in one transaction, and the next job of the leaf starts on the
// adopting lane, in its own tree, from the task branch head.
#[gpui::test]
async fn adopting_a_halted_task_cancels_its_run_and_starts_it_on_the_adopting_lane(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, true).await;
    let app = harness.host.fixture.app.clone();
    let failed_branch = run_branch_name(TASK_BRANCH, &failed, 1);
    // The save is on the run branch before the run is cancelled.
    let at_cancel = Rc::new(RefCell::new(Vec::new()));
    let seen = at_cancel.clone();
    let branch = failed_branch.clone();
    let repository = app.clone();
    *harness.engine.on_cancel.borrow_mut() = Some(Box::new(move |_run: &str| {
        seen.borrow_mut().push(subject(&repository, &branch));
    }));

    let adopted = harness
        .adopt(request("a", 2, "build-1's drive is slow"), cx)
        .await
        .unwrap();

    assert_eq!(adopted.from, Some(build(1)));
    assert_eq!(adopted.cancelled.as_deref(), Some(failed.as_str()));
    assert!(!adopted.dropped_work);
    assert_eq!(*harness.engine.cancelled.borrow(), [failed.as_str()]);
    assert_eq!(
        at_cancel.borrow().clone(),
        [format!("work left by failed run {failed}")]
    );
    let saved_head = git(&app, &["rev-parse", &failed_branch]);
    let history = harness.history("a");
    let expected = [
        format!(
            "work left on {failed_branch} at {}; too large to save: big.bin (2000 bytes)",
            &saved_head[..12]
        ),
        format!("run {} cancelled by adopt to build-2", short_run(&failed)),
        "adopted by build-2 from build-1: build-1's drive is slow (kim)".to_string(),
    ];
    assert_eq!(history[history.len() - 3..], expected);
    assert_eq!(adopted.saved, expected[..1]);

    let state = harness.state();
    let owner = &state.owners[&TaskId::new("a")];
    assert_eq!(owner.state, OwnerState::Active);
    assert_eq!(owner.lane, Some(build(2)));
    assert_eq!(owner.by, kim());
    let source = &state.lanes[&build(1)];
    assert_eq!(source.state, LaneState::Idle);
    assert_eq!(source.job, None);

    // The adopting lane runs the leaf in its own tree, from the task branch head.
    let target = &state.lanes[&build(2)];
    assert_eq!(target.state, LaneState::Running);
    let run = target.job.as_ref().unwrap().run_id.clone();
    let jobs = harness.engine.jobs.borrow();
    let (started, _) = jobs.last().unwrap();
    assert_eq!(started.run_id.as_deref(), Some(run.as_str()));
    assert_eq!(started.task_id(), Some("a"));
    assert_eq!(started.lane.as_ref().unwrap().id, "build-2");
    let tip = harness.host.tip(2);
    assert!(matches!(&started.tree, RunTree::Lane { path, .. } if *path == tip));
    assert_eq!(
        git(&tip, &["rev-parse", "HEAD"]),
        git(&app, &["rev-parse", TASK_BRANCH])
    );
    assert_eq!(
        git(&tip, &["branch", "--show-current"]),
        run_branch_name(TASK_BRANCH, &run, 1)
    );
    assert!(!tip.join("work.txt").exists());
}

// The cancelled run cannot bring the old lane back: a retry is refused by the engine (only
// a failed run is retried), and a status change of that run reaches no lane.
#[gpui::test]
async fn the_cancelled_run_never_gives_the_old_lane_its_task_back(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, false).await;
    harness.adopt(request("a", 2, "move it"), cx).await.unwrap();
    assert_eq!(
        harness.engine.status.borrow()[&failed].1,
        RunStatus::Cancelled
    );

    harness.set(&failed, RunStatus::Running, cx);

    let state = harness.state();
    assert_eq!(state.lanes[&build(1)].state, LaneState::Idle);
    assert_eq!(state.lanes[&build(1)].job, None);
    assert_eq!(state.owners[&TaskId::new("a")].lane, Some(build(2)));
    assert!(
        !harness
            .history("a")
            .iter()
            .any(|line| line.starts_with("resumed by retry"))
    );
}

// A file the failed run left uncommitted survives the source lane's next job, committed
// on the old run branch the history names.
#[gpui::test]
async fn work_left_in_the_source_tree_survives_its_next_job_on_the_old_run_branch(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, true).await;
    harness.adopt(request("a", 2, "move it"), cx).await.unwrap();
    let app = harness.host.fixture.app.clone();
    let failed_branch = run_branch_name(TASK_BRANCH, &failed, 1);
    let saved = git(&app, &["rev-parse", &failed_branch]);
    assert!(harness.history("a").contains(&format!(
        "work left on {failed_branch} at {}; too large to save: big.bin (2000 bytes)",
        &saved[..12]
    )));

    // The source lane takes other work: its tree is reset for it.
    harness.host.tasks.borrow_mut().push(leaf("b"));
    cx.update(|cx| harness.dispatcher().pick_now(cx));
    cx.run_until_parked();
    let next = harness.job_run(1);
    let tip = harness.host.tip(1);
    assert_eq!(
        git(&tip, &["branch", "--show-current"]),
        run_branch_name(TASK_BRANCH, &next, 1)
    );
    assert!(!tip.join("work.txt").exists() && !tip.join("big.bin").exists());
    // The reset saved nothing more: the work was already on the run branch.
    assert_eq!(
        harness.host.start_saves.borrow().clone(),
        ["no work left to save; too large to save: big.bin (2000 bytes)"]
    );

    assert_eq!(git(&app, &["rev-parse", &failed_branch]), saved);
    assert_eq!(
        subject(&app, &saved),
        format!("work left by failed run {failed}")
    );
    assert_eq!(
        git(&app, &["show", &format!("{saved}:work.txt")]),
        "half done"
    );
    assert_eq!(
        git(&app, &["show", &format!("{saved}:README.md")]),
        "edited"
    );
}

// A source tree with no uncommitted change saves nothing: the adopt only cancels and reassigns.
#[gpui::test]
async fn a_clean_source_tree_saves_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, false).await;
    let app = harness.host.fixture.app.clone();
    let failed_branch = run_branch_name(TASK_BRANCH, &failed, 1);
    let before = git(&app, &["rev-parse", &failed_branch]);

    let adopted = harness.adopt(request("a", 2, "move it"), cx).await.unwrap();

    assert!(adopted.saved.is_empty());
    assert_eq!(git(&app, &["rev-parse", &failed_branch]), before);
    assert_eq!(harness.engine.cancelled.borrow().clone(), [failed]);
    assert!(
        !harness
            .history("a")
            .iter()
            .any(|line| line.starts_with("work left on"))
    );
    assert_eq!(harness.state().lanes[&build(2)].state, LaneState::Running);
}

// With the old tree's directory missing, the adopt is refused unless the person confirms
// dropping its work; the confirmed drop is in the history.
#[gpui::test]
async fn a_missing_old_tree_refuses_the_adopt_unless_the_drop_is_confirmed(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, true).await;
    std::fs::remove_dir_all(harness.host.tip(1)).unwrap();
    let before = harness.state();

    let error = harness
        .adopt(request("a", 2, "drive gone"), cx)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot be read"), "{error}");
    assert!(error.contains("confirm to drop that work"), "{error}");
    assert!(harness.engine.cancelled.borrow().is_empty());
    assert_eq!(harness.state(), before);

    let mut confirmed = request("a", 2, "drive gone");
    confirmed.confirm_drop = true;
    let adopted = harness.adopt(confirmed, cx).await.unwrap();
    assert!(adopted.dropped_work);
    assert_eq!(harness.engine.cancelled.borrow().clone(), [failed]);
    let history = harness.history("a");
    assert_eq!(
        history.last().unwrap(),
        "adopted by build-2 from build-1: drive gone (kim); unreadable old work dropped"
    );
    assert_eq!(
        harness.state().owners[&TaskId::new("a")].lane,
        Some(build(2))
    );
}

// A failed save (here a tree git cannot read) cancels nothing, moves nothing and deletes nothing,
// even with a drop confirmed.
#[gpui::test]
async fn a_failed_save_leaves_everything_as_it_was(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    let failed = harness.halted(cx, true).await;
    let tip = harness.host.tip(1);
    let link = std::fs::read_to_string(tip.join(".git")).unwrap();
    std::fs::write(tip.join(".git"), "gitdir: /nonexistent/broken\n").unwrap();
    let before = harness.state();
    let history = harness.history("a");

    let mut confirmed = request("a", 2, "move it");
    confirmed.confirm_drop = true;
    let error = harness.adopt(confirmed, cx).await.unwrap_err().to_string();

    assert!(error.contains("was not saved"), "{error}");
    assert!(harness.engine.cancelled.borrow().is_empty());
    assert_eq!(harness.engine.status.borrow()[&failed].1, RunStatus::Failed);
    assert_eq!(harness.state(), before);
    assert_eq!(harness.history("a"), history);
    assert_eq!(
        std::fs::read_to_string(tip.join("work.txt")).unwrap(),
        "half done\n"
    );
    std::fs::write(tip.join(".git"), link).unwrap();
    assert_eq!(
        git(&tip, &["status", "--porcelain", "--", "work.txt"]),
        "?? work.txt"
    );
}

// A person's `workflow: cancel` of a lane job with uncommitted work, then the lane's next job:
// the work is saved on the cancelled run's branch before the reset.
#[gpui::test]
async fn work_of_a_cancelled_job_is_on_its_run_branch_after_the_next_job(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut harness = TreeHarness::new();
    harness.start(cx).await;
    let cancelled = harness.job_run(1);
    let tip = harness.host.tip(1);
    std::fs::write(tip.join("work.txt"), "half done\n").unwrap();

    harness.set(&cancelled, RunStatus::Cancelled, cx);
    assert_eq!(harness.state().lanes[&build(1)].state, LaneState::Paused);
    cx.update(|cx| {
        harness.lanes.resume(&build(1)).unwrap();
        harness.dispatcher().pick_now(cx);
    });
    cx.run_until_parked();

    let next = harness.job_run(1);
    assert_ne!(next, cancelled);
    let app = harness.host.fixture.app.clone();
    let branch = run_branch_name(TASK_BRANCH, &cancelled, 1);
    let saved = git(&app, &["rev-parse", &branch]);
    assert_eq!(
        subject(&app, &saved),
        format!("work left by cancelled run {cancelled}")
    );
    assert_eq!(
        git(&app, &["show", &format!("{saved}:work.txt")]),
        "half done"
    );
    assert_eq!(
        harness.host.start_saves.borrow().clone(),
        [format!("work left on {branch} at {}", &saved[..12])]
    );
    assert!(!tip.join("work.txt").exists());
}

/// The error of an adopt the checks refuse.
async fn refused(
    harness: &super::Harness,
    task: &str,
    lane: LaneId,
    reason: &str,
    cx: &mut TestAppContext,
) -> String {
    let adopting = cx.update(|cx| {
        harness.lanes.adopt(
            AdoptRequest {
                task: TaskId::new(task),
                lane,
                reason: reason.to_string(),
                by: kim(),
                confirm_drop: false,
            },
            cx,
        )
    });
    match adopting.await {
        Ok(adopted) => panic!("{task} was adopted: {adopted:?}"),
        Err(error) => error.to_string(),
    }
}

// The refusals, each before anything is written or cancelled.
#[gpui::test]
async fn adopt_refuses_a_running_owner_a_pin_a_busy_lane_and_no_reason(cx: &mut TestAppContext) {
    let table = format!(
        "{}[[kind]]\nname = \"docs\"\ncount = 1\n[[pin]]\ntask = \"p\"\nlane = \"build-1\"\n\
         [[pin]]\ntask = \"q\"\nkind = \"docs\"\n",
        builds(3)
    );
    let harness = harness(&table, graph(vec![leaf("a")]), &[]);
    let _started = super::start(&harness, start_request(), cx).await;
    assert_eq!(harness.lane("build", 1).state, LaneState::Running);
    // Stopped tasks whose lane is gone: any lane may be given them, pins and lanes allowing.
    let stopped = bongos_lanes_model::OwnerRecord {
        lane: None,
        state: OwnerState::Stopped,
        since: SINCE.to_string(),
        by: Author::System,
        reason: Some("lane count lowered".to_string()),
        prepared_commit: None,
        refreshed: false,
    };
    let mut paused = harness.lane("build", 3);
    paused.state = LaneState::Paused;
    harness
        .writer
        .transact(|tx| {
            for task in ["p", "q", "h"] {
                put_owner(tx, &TaskId::new(task), &stopped)?;
            }
            put_lane(tx, &paused)
        })
        .unwrap();
    cx.run_until_parked();
    let before = harness.state();

    assert_eq!(
        refused(&harness, "a", LaneId::new("docs", 1), "why", cx).await,
        "a is owned by build-1, which is running"
    );
    assert_eq!(
        refused(&harness, "p", LaneId::new("docs", 1), "why", cx).await,
        "p is pinned elsewhere, so docs-1 cannot take it"
    );
    assert_eq!(
        refused(&harness, "q", build(1), "why", cx).await,
        "q is pinned elsewhere, so build-1 cannot take it"
    );
    assert_eq!(
        refused(&harness, "h", build(3), "why", cx).await,
        "build-3 is not free: it is not idle"
    );
    assert_eq!(
        refused(&harness, "h", build(1), "why", cx).await,
        "build-1 is not free: it owns a"
    );
    assert_eq!(
        refused(&harness, "h", LaneId::new("docs", 1), "  ", cx).await,
        "an adopt needs a reason"
    );
    assert!(harness.engine.cancelled.borrow().is_empty());
    assert_eq!(harness.state(), before);
    assert!(
        harness
            .writer
            .read(|read| read.task("h"))
            .unwrap()
            .is_none_or(|record| record.history.is_empty())
    );
}
