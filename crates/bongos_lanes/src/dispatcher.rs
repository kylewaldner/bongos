//! The dispatcher: lanes pick, start and end jobs in one loop.
//!
//! [`Lanes::start`] checks the project, starts the dispatcher's own run (a project run of the
//! dispatcher definition), records it, creates the lane records up to the effective counts and
//! evaluates. A pass ([`Dispatcher::evaluate`]) walks the lanes in index order; for each free lane
//! it takes a graph snapshot and opens one state transaction that reads the lanes state, picks,
//! writes the owner and lane records and reserves the job's run id. The transaction refuses to
//! write when the store's generation moved since the snapshot, and the lane is evaluated again.
//! Only after the commit is the run started ([`crate::dispatch_job`]); a refusal is cleared in a
//! second transaction. Because the choice, the area check, the instance count and the ownership
//! records are in one transaction of the one writer, a second lane freeing at the same time sees
//! the first one's records.
//!
//! Passes run on: a run's status change, a committed state transaction, a new verified commit, a
//! lane command, and every `lanes.reevaluate_interval`. Each pass first clears the `Done` owners
//! that may go ([`crate::done_owners`]), so a write of the checks' markers or done records, a
//! committed transaction like any other, re-checks them. A lane waiting on its environment is
//! looked at again only by the timer, a new verified commit or a lane command, so a write of this
//! loop never wakes the loop into retrying the same failing environment.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use bongos_lanes_model::{
    AssignedWork, Assignment, Author, JobRecord, LaneId, LaneRecord, LaneState, LaneTable,
    LanesState, OwnerChange, OwnerRecord, OwnerState, PickBound, PickGraph, PickOptions,
    PickOutcome, Problem, RunSubject, StampFile, StopCause, TaskId, WaitReason, owner_after_stop,
    pick, reconcile_counts, stop_at_job_boundary,
};
use bongos_project_repo::{CommitId, RepoEvent, StateTx, StateWriter};
use bongos_workflow::lifecycle::RefusalKind;
use bongos_workflow::{Refusal, RunId, RunTree, StartRequest, WorkflowEngine, WorkflowEvent};
use bongos_workflow_model::record::StartedBy;
use bongos_workflow_model::{EnvEdit, UnattendedChoice, UnattendedSource};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, Task};
use serde::{Deserialize, Serialize};

use crate::adopt::LeftWork;
use crate::dispatch_job::HookedJob;
use crate::job_trees::JobTrees;
use crate::lane_control::{BOUND_REASON, LaneEvents, RunEnd, paused_reason};
use crate::locations::table_lane_ids;
use crate::pick_extension::{
    ExtensionPick, JobChange, PickExtension, PickExtensions, PickPoint, extension_job_changed,
    extension_pick, instance_use, lane_after_unstarted, with_engine,
};
use crate::profile::LaneProfiles;
use crate::store::{LANES_NAMESPACE, delete_lane, put_lane, put_owner, read_state_in};
use crate::task_trees::LaneTreeSetup;

/// The key of the dispatcher record in the lanes namespace.
pub const DISPATCHER_KEY: &str = "dispatcher";
/// The dispatcher definition run when the lane table names none.
pub const DEFAULT_DISPATCHER: &str = "lanes";
/// How many times one lane is evaluated again in a pass because the state moved under it.
const MAX_RESTARTS: usize = 8;

/// What `lanes: start` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartLanes {
    pub bound: Option<PickBound>,
    /// A lane stops after this many jobs of this start.
    pub max_jobs: Option<u32>,
    /// Whether the lanes' runs run unattended; `None` leaves it to the lane kind or the project.
    pub unattended: Option<bool>,
    pub by: Author,
}

/// A bound as the dispatcher record keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedBound {
    Until(TaskId),
    Within(TaskId),
}

impl From<&PickBound> for RecordedBound {
    fn from(bound: &PickBound) -> Self {
        match bound {
            PickBound::Until(task) => Self::Until(task.clone()),
            PickBound::Within(task) => Self::Within(task.clone()),
        }
    }
}

/// The dispatcher's run and the host running it, under [`DISPATCHER_KEY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatcherRecord {
    pub run_id: String,
    /// The process (engine host or window) running the dispatcher.
    pub host: String,
    pub started: String,
    pub by: Author,
    #[serde(default)]
    pub bound: Option<RecordedBound>,
    #[serde(default)]
    pub unattended: Option<UnattendedChoice>,
    #[serde(default)]
    pub definition_commit: Option<String>,
    /// Who asked every lane to stop.
    #[serde(default)]
    pub stopped_by: Option<Author>,
    /// How the run ended, once it did.
    #[serde(default)]
    pub ended: Option<RunEnd>,
}

/// Reads the dispatcher record in `tx`.
pub fn dispatcher_record(tx: &mut StateTx) -> Result<Option<DispatcherRecord>> {
    tx.kv(LANES_NAMESPACE)
        .get(DISPATCHER_KEY)?
        .map(|value| value.try_into().context("reading the dispatcher record"))
        .transpose()
}

/// Writes the dispatcher record.
pub fn put_dispatcher(tx: &mut StateTx, record: &DispatcherRecord) -> Result<()> {
    let value = toml::Value::try_from(record).context("serializing the dispatcher record")?;
    tx.kv(LANES_NAMESPACE).put(DISPATCHER_KEY, value)
}

/// What a lane's job runs in, from the job's trees and the lane's environment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobSetup {
    pub tree: RunTree,
    pub env: Vec<EnvEdit>,
    pub lane_dirs: BTreeMap<String, PathBuf>,
    /// The trees of the job, kept in [`crate::CurrentJobTrees`] once it starts.
    pub trees: Option<JobTrees>,
    /// What the lane's trees offer outside the job, from the same start; set in the app's
    /// [`crate::LaneTaskTrees`] once the run starts.
    pub lane_trees: Option<LaneTreeSetup>,
}

impl JobSetup {
    /// The setup of a job in `trees` (from [`crate::start_job_trees`]) with the lane's
    /// environment edits (from [`crate::tip_env`]) and named directories.
    pub fn in_lane_tree(
        trees: JobTrees,
        env: Vec<EnvEdit>,
        lane_dirs: BTreeMap<String, PathBuf>,
    ) -> Self {
        Self {
            tree: RunTree::Lane {
                path: trees.primary().to_path_buf(),
                base: None,
                start: None,
                merge_into: None,
            },
            env,
            lane_dirs,
            trees: Some(trees),
            lane_trees: None,
        }
    }

    /// This setup with what the lane's trees offer outside the job. `setup.second_checked`
    /// comes from the same start's [`crate::StartedTrees::second_checked`].
    pub fn with_lane_trees(mut self, setup: LaneTreeSetup) -> Self {
        self.lane_trees = Some(setup);
        self
    }
}

/// What the app tells the dispatcher about its project.
pub trait LanesHost {
    fn project_id(&self) -> String;

    /// Whether the project keeps its definitions in a project repository; only such a project
    /// has lanes.
    fn is_repository_project(&self) -> bool;

    /// Reads, off the main thread, what [`Self::lane_table`] and [`Self::stamp`] answer at
    /// `commit`. Every caller awaits it before asking either of them about a commit, so they
    /// never run git on the main thread.
    fn load_commit(&self, _commit: &str, _cx: &mut App) -> Task<()> {
        Task::ready(())
    }

    /// The lane table at the verified commit `commit`, once [`Self::load_commit`] read it.
    fn lane_table(&self, commit: &str) -> Result<LaneTable, String>;

    /// Resolves the locations of `lanes` and checks that no two overlap
    /// ([`crate::check_lanes`]).
    fn check_locations(
        &self,
        table: &LaneTable,
        lanes: &[LaneId],
        cx: &App,
    ) -> Result<(), Vec<Problem>>;

    /// A snapshot of the task graph with the host's live facts; `None` when the project is not
    /// loaded.
    fn graph(&self, cx: &App) -> Option<PickGraph>;

    /// This process, as a dispatcher record names its host.
    fn host_id(&self) -> String;

    /// Whether the host a dispatcher record names is alive.
    fn host_live(&self, host: &str) -> bool;

    /// The time now, RFC 3339.
    fn now(&self) -> String;

    /// The time between two timed passes.
    fn reevaluate_interval(&self, cx: &App) -> Duration {
        use settings::Settings as _;
        crate::LanesSettings::get_global(cx).reevaluate_interval
    }

    /// The repository's events; `None` when there is no repository to listen to.
    fn repo_events(&self) -> Option<mpsc::UnboundedReceiver<RepoEvent>> {
        None
    }

    /// Prepares the trees and environment of `job` on `lane`. An error is an environment wait:
    /// no space, a tree that cannot be made. With nothing to prepare a job runs in a fresh run
    /// worktree with no lane environment.
    fn prepare_job(
        &self,
        _lane: &LaneId,
        _job: &JobRecord,
        _cx: &mut App,
    ) -> Task<Result<JobSetup, WaitReason>> {
        Task::ready(Ok(JobSetup::default()))
    }

    /// The lane's persistent trees `job` ran in, each with what saving the work it left there
    /// needs (`lanes: adopt` saves it before cancelling the job's run). The request's `last` names
    /// `job`'s run. With no persistent tree (fresh run worktrees, kept by the run) there is none.
    /// Asked after [`Self::load_commit`] of the verified commit; any git it needs runs in the task.
    fn left_work(&self, _lane: &LaneId, _job: &JobRecord, _cx: &App) -> Task<Vec<LeftWork>> {
        Task::ready(Vec::new())
    }

    /// The recorded graph stamp (`lanes/stamp.toml`) at the verified commit `commit`; `None` when
    /// there is none, which turns the stamp check off. Asked after [`Self::load_commit`].
    fn stamp(&self, _commit: &str) -> Result<Option<StampFile>, String> {
        Ok(None)
    }
}

/// The run engine as the dispatcher uses it: [`WorkflowEngine`] in the app.
pub trait JobEngine {
    fn start(&self, request: StartRequest, cx: &mut App) -> Result<RunId, Refusal>;
    /// Whether a run with this id is loaded in the engine now.
    fn knows(&self, run: &str) -> bool;
    /// Whether a run with this id has a record on disk, loaded or not: `false` only for a run
    /// that never started. One the engine could not load (a damaged log, a duplicate) still has
    /// its record. When it cannot be told this is `true`, so a job is never taken for unstarted
    /// on doubt.
    fn has_record(&self, run: &str, cx: &App) -> bool;
    /// The active runs of the workflow definition `workflow`.
    fn active_runs_of(&self, workflow: &str) -> Vec<RunId>;
    /// The definition's `instances`; `None` is unlimited.
    fn instance_limit(&self, workflow: &str) -> Option<u32>;
    /// Calls `f` with every run status change from now on.
    fn observe(&self, f: Box<dyn Fn(&WorkflowEvent, &mut App)>, cx: &mut App);
    /// Ends the dispatcher's own run: cancelled when every lane was stopped on request, succeeded
    /// when every lane stopped for its bound. `note` says why.
    fn end_dispatcher(&self, run: &str, end: RunEnd, note: &str, cx: &mut App);
    /// Cancels the run of a lane's job (`lanes: adopt` closes out a halted job's failed run).
    fn cancel_run(&self, run: &str, cx: &mut App) -> Result<(), String>;
}

impl JobEngine for WorkflowEngine {
    fn start(&self, request: StartRequest, cx: &mut App) -> Result<RunId, Refusal> {
        WorkflowEngine::start(self, request, cx)
    }

    fn knows(&self, run: &str) -> bool {
        self.lifecycle().status(run).is_some()
    }

    fn has_record(&self, run: &str, cx: &App) -> bool {
        self.run_recorded(run, cx).unwrap_or(true)
    }

    fn active_runs_of(&self, workflow: &str) -> Vec<RunId> {
        let lifecycle = self.lifecycle();
        lifecycle
            .active_runs()
            .into_iter()
            .filter(|run| {
                lifecycle
                    .record(run)
                    .is_some_and(|record| record.workflow == workflow)
            })
            .collect()
    }

    fn instance_limit(&self, workflow: &str) -> Option<u32> {
        let resolved =
            bongos_workflow::definitions::find_definition(self.dirs(), Some(workflow)).ok()?;
        let text = std::fs::read_to_string(
            resolved
                .dir
                .join(bongos_workflow_model::layers::DEFINITION_FILE),
        )
        .ok()?;
        bongos_workflow_model::parse(&text).ok()?.instances
    }

    fn observe(&self, f: Box<dyn Fn(&WorkflowEvent, &mut App)>, cx: &mut App) {
        WorkflowEngine::observe(cx, f);
    }

    fn end_dispatcher(&self, run: &str, end: RunEnd, note: &str, cx: &mut App) {
        match end {
            RunEnd::Cancelled => {
                log::info!("lanes: cancelling the dispatcher run {run}: {note}");
                if let Err(refusal) = self.cancel(&run.to_string(), cx) {
                    log::error!("lanes: cancelling the dispatcher run {run}: {refusal}");
                }
            }
            // The engine has no way to finish a run on its own yet; the run stays active.
            RunEnd::Succeeded => log::info!("lanes: the dispatcher run {run} is done: {note}"),
        }
    }

    fn cancel_run(&self, run: &str, cx: &mut App) -> Result<(), String> {
        WorkflowEngine::cancel(self, &run.to_string(), cx).map_err(|refusal| refusal.to_string())
    }
}

/// Which commit's definitions a lane's next job reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobCommit {
    Use(String),
    /// The lane stays idle with this reason.
    Wait(String),
    /// The lane stops first (a rollout).
    StopFirst(String),
}

/// Decides the definition commit of a lane's next job. A rollout registers its own; the default
/// always uses the verified commit.
pub trait JobDefinitionPolicy {
    fn commit_for_job(&self, _lane: &LaneId, master: &str) -> JobCommit {
        JobCommit::Use(master.to_string())
    }
}

/// The policy with no rollout: every job reads the verified commit.
pub struct VerifiedCommitPolicy;

impl JobDefinitionPolicy for VerifiedCommitPolicy {}

/// Everything a dispatcher runs over.
#[derive(Clone)]
pub struct LanesParts {
    pub host: Rc<dyn LanesHost>,
    pub engine: Rc<dyn JobEngine>,
    pub policy: Rc<dyn JobDefinitionPolicy>,
    pub writer: Arc<StateWriter>,
}

/// Why `lanes: start` refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StartRefusal {
    #[error("lanes need a project with a project repository")]
    NotRepositoryProject,
    #[error("the project profile refused the dispatcher: {}", problems(.0))]
    Profile(Vec<Problem>),
    #[error("the project has no verified commit yet")]
    NoVerifiedCommit,
    #[error("the lane table cannot be read: {0}")]
    Table(String),
    #[error("the lane locations are not usable: {}", problems(.0))]
    Locations(Vec<Problem>),
    #[error("the dispatcher run did not start: {0}")]
    Engine(Refusal),
    #[error("the state store failed: {0}")]
    Store(String),
}

fn problems(list: &[Problem]) -> String {
    list.iter()
        .map(|problem| problem.message.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

fn store_error(error: anyhow::Error) -> StartRefusal {
    StartRefusal::Store(format!("{error:#}"))
}

/// What a start did.
pub struct LanesStarted {
    pub run_id: String,
    /// A live host already runs the dispatcher; this start attached to its run.
    pub attached: bool,
    /// The dispatcher this process runs; `None` when attached.
    pub dispatcher: Option<Dispatcher>,
}

/// The lanes of one project.
pub struct Lanes {
    pub parts: LanesParts,
    pub(crate) events: LaneEvents,
    /// The dispatcher this handle last started, while it lives.
    pub(crate) started: Rc<RefCell<Weak<Inner>>>,
}

impl Lanes {
    pub fn new(parts: LanesParts) -> Self {
        Self {
            parts,
            events: LaneEvents::default(),
            started: Rc::default(),
        }
    }

    /// The live dispatcher this handle started, if any.
    pub(crate) fn dispatcher(&self) -> Option<Dispatcher> {
        self.started.borrow().upgrade().map(Dispatcher)
    }

    /// Starts the lanes, or attaches to the dispatcher a live host runs.
    pub fn start(&self, request: StartLanes, cx: &mut App) -> Task<Result<LanesStarted>> {
        let parts = self.parts.clone();
        let events = self.events.clone();
        let started = self.started.clone();
        let host = parts.host.clone();
        if !host.is_repository_project() {
            return Task::ready(Err(StartRefusal::NotRepositoryProject.into()));
        }
        let project = host.project_id();
        let profile = LaneProfiles::source(cx);
        let verify = profile
            .as_ref()
            .map(|profile| profile.verify_dispatcher(&project, cx));
        cx.spawn(async move |cx| {
            if let Some(verify) = verify {
                verify.await.map_err(StartRefusal::Profile)?;
            }
            if let Ok(Some(commit)) = parts.writer.read(|read| read.verified_commit()) {
                let loading = cx.update(|cx| host.load_commit(&commit, cx));
                loading.await;
            }
            let result = cx.update(|cx| start_verified(parts, events, project, request, cx));
            if let Ok(LanesStarted {
                dispatcher: Some(dispatcher),
                ..
            }) = &result
            {
                *started.borrow_mut() = dispatcher.downgrade();
            }
            result.map_err(Into::into)
        })
    }
}

fn start_verified(
    parts: LanesParts,
    events: LaneEvents,
    project: String,
    request: StartLanes,
    cx: &mut App,
) -> Result<LanesStarted, StartRefusal> {
    let host = parts.host.clone();
    let profile = LaneProfiles::source(cx);
    let verified = parts
        .writer
        .read(|read| read.verified_commit())
        .map_err(store_error)?
        .ok_or(StartRefusal::NoVerifiedCommit)?;
    let table = host.lane_table(&verified).map_err(StartRefusal::Table)?;
    let lanes = table_lane_ids(&table, &project, profile.as_deref());
    host.check_locations(&table, &lanes, cx)
        .map_err(StartRefusal::Locations)?;

    let existing = parts
        .writer
        .transact(dispatcher_record)
        .map_err(store_error)?;
    if let Some(record) = existing
        && host.host_live(&record.host)
    {
        return Ok(LanesStarted {
            run_id: record.run_id,
            attached: true,
            dispatcher: None,
        });
    }

    let unattended = request.unattended.map(|value| UnattendedChoice {
        value,
        source: UnattendedSource::Start,
    });
    let mut start = StartRequest::project(project.clone());
    start.workflow = Some(
        table
            .dispatcher
            .clone()
            .unwrap_or_else(|| DEFAULT_DISPATCHER.to_string()),
    );
    start.started_by = match request.by {
        Author::Person(_) => StartedBy::Person,
        Author::Lane(_) | Author::System => StartedBy::Host,
    };
    start.unattended = Some(unattended.unwrap_or(PROJECT_UNATTENDED));
    start.definition_commit = Some(CommitId(verified.clone()));
    let run_id = parts
        .engine
        .start(start, cx)
        .map_err(StartRefusal::Engine)?;

    let counts = effective_counts(&table, &project, profile.as_deref());
    let stamp = read_stamp(host.as_ref(), &verified);
    let now = host.now();
    let record = DispatcherRecord {
        run_id: run_id.clone(),
        host: host.host_id(),
        started: now.clone(),
        by: request.by.clone(),
        bound: request.bound.as_ref().map(RecordedBound::from),
        unattended,
        definition_commit: Some(verified.clone()),
        stopped_by: None,
        ended: None,
    };
    let extensions = PickExtensions::extensions(cx);
    let app: &App = cx;
    with_engine(&parts.engine, || {
        parts.writer.transact(|tx| {
            put_dispatcher(tx, &record)?;
            clear_unstarted(tx, &*parts.engine, &extensions, &now, app)?;
            reconcile(tx, &counts)
        })
    })
    .map_err(store_error)?;

    let dispatcher = Dispatcher::new(DispatcherConfig {
        parts,
        project,
        run_id: run_id.clone(),
        table,
        counts,
        bound: request.bound,
        max_jobs: request.max_jobs,
        unattended,
        verified,
        stamp,
        events,
    });
    dispatcher.run(cx);
    dispatcher.evaluate(true, cx);
    Ok(LanesStarted {
        run_id,
        attached: false,
        dispatcher: Some(dispatcher),
    })
}

/// The lane count of each kind of `table`: the profile's, else the table's.
fn effective_counts(
    table: &LaneTable,
    project: &str,
    profile: Option<&dyn crate::LaneProfileSource>,
) -> BTreeMap<String, u32> {
    table
        .kinds
        .iter()
        .map(|kind| {
            let count = profile
                .and_then(|profile| profile.count(project, &kind.name))
                .unwrap_or(kind.count);
            (kind.name.clone(), count)
        })
        .collect()
}

/// The recorded stamp at `commit`; one that cannot be read turns the check off, logged.
fn read_stamp(host: &dyn LanesHost, commit: &str) -> Option<StampFile> {
    host.stamp(commit).unwrap_or_else(|error| {
        log::error!(
            "lanes: the graph stamp at {commit} cannot be read, so it is not checked: {error}"
        );
        None
    })
}

/// The unattended choice of a run when nothing sets one.
pub const PROJECT_UNATTENDED: UnattendedChoice = UnattendedChoice {
    value: true,
    source: UnattendedSource::Project,
};

/// The reason an extension hears for a job a crash left with no run.
pub const NOT_STARTED_BEFORE_A_CRASH: &str = "not started before a crash";

/// Clears every lane job whose reserved run never started (a crash came between the pick and the
/// start): the lane is idle again and its owner record stays, so the same lane continues the
/// task. An extension job's extensions hear [`JobChange::NotStarted`] first and may decide the
/// lane instead. Only called when no live host runs a dispatcher, so no start is in flight.
///
/// A job is unstarted only when its run has no record on disk ([`JobEngine::has_record`]), never
/// merely because the engine did not load the run: a started run the engine skipped (a damaged
/// log) keeps its job, so an extension never hands out the same work twice.
fn clear_unstarted(
    tx: &mut StateTx,
    engine: &dyn JobEngine,
    extensions: &[Arc<dyn PickExtension>],
    now: &str,
    cx: &App,
) -> Result<()> {
    let state = read_state_in(tx)?;
    for (id, lane) in &state.lanes {
        let Some(job) = &lane.job else {
            continue;
        };
        if engine.has_record(&job.run_id, cx) {
            continue;
        }
        log::info!("lanes: {id} reserved run {} that never started", job.run_id);
        let change = JobChange::NotStarted {
            reason: NOT_STARTED_BEFORE_A_CRASH.to_string(),
        };
        if let Some(after) = extension_job_changed(extensions, tx, id, job, change) {
            lane_after_unstarted(tx, id, &job.run_id, after, now)?;
            continue;
        }
        let mut updated = lane.clone();
        updated.job = None;
        updated.state = LaneState::Idle;
        updated.reason = None;
        put_lane(tx, &updated)?;
    }
    Ok(())
}

/// Creates the lane records up to `counts`, removes the free lanes above them and asks the
/// running ones above them to stop after their job.
fn reconcile(tx: &mut StateTx, counts: &BTreeMap<String, u32>) -> Result<()> {
    let state = read_state_in(tx)?;
    let plan = reconcile_counts(counts, &state);
    for id in &plan.create {
        put_lane(tx, &LaneRecord::new(id, LaneState::Idle))?;
    }
    for id in &plan.remove_now {
        delete_lane(tx, id)?;
    }
    for id in &plan.stop_after_job {
        if let Some(lane) = state.lanes.get(id) {
            let mut lane = lane.clone();
            lane.stop_requested = Some(StopCause::Count);
            put_lane(tx, &lane)?;
        }
    }
    // A new start makes a stopped lane idle again (it takes its own stopped task first) and
    // forgets a stop recorded on a halted or paused lane. A halted or paused lane stays as it is.
    for (id, lane) in &state.lanes {
        if plan.remove_now.contains(id) || plan.stop_after_job.contains(id) {
            continue;
        }
        let mut updated = lane.clone();
        if updated.stop_requested == Some(StopCause::Stop) {
            updated.stop_requested = None;
        }
        if updated.state == LaneState::Stopped {
            updated.state = LaneState::Idle;
            updated.reason = None;
            updated.stop_requested = None;
        }
        if updated != *lane {
            put_lane(tx, &updated)?;
        }
    }
    Ok(())
}

/// What a dispatcher is made from.
pub struct DispatcherConfig {
    pub parts: LanesParts,
    pub project: String,
    /// The dispatcher's own run, every job's parent.
    pub run_id: String,
    pub table: LaneTable,
    /// The effective lane count of each kind.
    pub counts: BTreeMap<String, u32>,
    pub bound: Option<PickBound>,
    pub max_jobs: Option<u32>,
    /// The start's unattended choice.
    pub unattended: Option<UnattendedChoice>,
    pub verified: String,
    /// The recorded graph stamp at `verified`.
    pub stamp: Option<StampFile>,
    pub events: LaneEvents,
}

/// The loop of one project's lanes. Cheap to clone.
#[derive(Clone)]
pub struct Dispatcher(pub(crate) Rc<Inner>);

pub(crate) struct Inner {
    pub(crate) config: DispatcherConfig,
    table: RefCell<LaneTable>,
    verified: RefCell<String>,
    /// The effective lane count of each kind, changed at a profile or table change.
    counts: RefCell<BTreeMap<String, u32>>,
    stamp: RefCell<Option<StampFile>>,
    /// Lanes held waiting because their locations overlap another lane's, with the reason.
    overlapping: RefCell<BTreeMap<LaneId, String>>,
    /// A pass is running.
    passing: Cell<bool>,
    /// A pass was asked for while one ran: `Some(full)`.
    again: Cell<Option<bool>>,
    passes: Cell<usize>,
    restarts: Cell<usize>,
    /// The dispatcher's run ended; no pass runs any more.
    ended: Cell<bool>,
    /// Each lane's `jobs_done` when this dispatcher first saw it, for `max_jobs`.
    baseline: RefCell<BTreeMap<LaneId, u32>>,
    triggers: RefCell<Vec<Task<()>>>,
    subscriptions: RefCell<Vec<gpui::Subscription>>,
    /// The jobs that passed the job hooks, by lane, until their `after_job` resolved. A lane
    /// listed here is not evaluated, and the dispatcher's run does not end while any is listed.
    ///
    /// Dropping the dispatcher drops these as they are: a pending `before_job` future is dropped
    /// and no hook gets its `after_job`. Whoever holds the dispatcher keeps it until its run
    /// ended, which is after every job here was settled.
    pub(crate) hooked: RefCell<BTreeMap<LaneId, HookedJob>>,
}

/// What one lane's transaction decided.
enum LaneStep {
    /// The store moved since the snapshot: evaluate the lane again.
    Restart,
    Nothing,
    Start(PendingJob),
}

/// A job whose records are committed and whose run is still to start.
#[derive(Debug, Clone)]
pub(crate) struct PendingJob {
    pub(crate) lane: LaneId,
    pub(crate) job: JobRecord,
    pub(crate) work: AssignedWork,
    pub(crate) inputs: Vec<(String, String)>,
}

impl Dispatcher {
    pub fn new(config: DispatcherConfig) -> Self {
        Self(Rc::new(Inner {
            table: RefCell::new(config.table.clone()),
            verified: RefCell::new(config.verified.clone()),
            counts: RefCell::new(config.counts.clone()),
            stamp: RefCell::new(config.stamp.clone()),
            overlapping: RefCell::new(BTreeMap::new()),
            config,
            passing: Cell::new(false),
            again: Cell::new(None),
            passes: Cell::new(0),
            restarts: Cell::new(0),
            ended: Cell::new(false),
            baseline: RefCell::new(BTreeMap::new()),
            triggers: RefCell::new(Vec::new()),
            subscriptions: RefCell::new(Vec::new()),
            hooked: RefCell::new(BTreeMap::new()),
        }))
    }

    pub(crate) fn downgrade(&self) -> Weak<Inner> {
        Rc::downgrade(&self.0)
    }

    pub(crate) fn parts(&self) -> &LanesParts {
        &self.0.config.parts
    }

    /// The dispatcher's own run.
    pub fn run_id(&self) -> &str {
        &self.0.config.run_id
    }

    /// How many passes ran, for tests.
    pub fn passes(&self) -> usize {
        self.0.passes.get()
    }

    /// How many lane evaluations started again because the state moved, for tests.
    pub fn restarts(&self) -> usize {
        self.0.restarts.get()
    }

    pub(crate) fn table(&self) -> LaneTable {
        self.0.table.borrow().clone()
    }

    /// Listens for the triggers of a pass: run status changes, committed state transactions,
    /// new verified commits and the timer.
    pub fn run(&self, cx: &mut App) {
        let parts = self.parts().clone();
        let weak = self.downgrade();
        parts.engine.observe(
            Box::new(move |event, cx| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let event = event.clone();
                cx.defer(move |cx| Dispatcher(inner).on_workflow_event(&event, cx));
            }),
            cx,
        );

        let mut triggers = Vec::new();
        let mut states = parts.writer.subscribe();
        let weak = self.downgrade();
        triggers.push(cx.spawn(async move |cx| {
            while states.next().await.is_some() {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                cx.update(|cx| Dispatcher(inner).evaluate(false, cx));
            }
        }));

        if let Some(mut events) = parts.host.repo_events() {
            let weak = self.downgrade();
            triggers.push(cx.spawn(async move |cx| {
                while let Some(event) = events.next().await {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    if let RepoEvent::VerifiedCommitChanged { commit } = event {
                        let dispatcher = Dispatcher(inner);
                        let loading =
                            cx.update(|cx| dispatcher.parts().host.load_commit(&commit, cx));
                        loading.await;
                        cx.update(|cx| dispatcher.verified_commit_changed(commit, cx));
                    }
                }
            }));
        }

        let interval = parts.host.reevaluate_interval(cx);
        let weak = self.downgrade();
        triggers.push(cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                cx.update(|cx| Dispatcher(inner).evaluate(true, cx));
            }
        }));
        self.0.triggers.borrow_mut().extend(triggers);

        let weak = self.downgrade();
        let profile_changed = cx.observe_global::<LaneProfiles>(move |cx| {
            if let Some(inner) = weak.upgrade() {
                Dispatcher(inner).counts_changed(cx);
            }
        });
        self.0.subscriptions.borrow_mut().push(profile_changed);
    }

    /// A new verified commit: its lane table, its stamp and the counts they give, then a full
    /// pass.
    pub fn verified_commit_changed(&self, commit: String, cx: &mut App) {
        match self.parts().host.lane_table(&commit) {
            Ok(table) => *self.0.table.borrow_mut() = table,
            Err(error) => log::error!("lanes: the lane table at {commit} cannot be read: {error}"),
        }
        *self.0.stamp.borrow_mut() = read_stamp(self.parts().host.as_ref(), &commit);
        *self.0.verified.borrow_mut() = commit;
        self.apply_counts(cx);
        self.evaluate(true, cx);
    }

    /// The project profile changed: its lane counts apply now, at each lane's job boundary.
    pub fn counts_changed(&self, cx: &mut App) {
        if self.apply_counts(cx) {
            self.evaluate(true, cx);
        }
    }

    /// The effective counts now, from the table and the profile, applied with no restart: lanes up
    /// to a raised count are made (held waiting while their locations overlap another lane's),
    /// free lanes above a lowered count are removed with their task stopped, and running ones
    /// stop after their job. A lane removed is never reset; its trees stay on disk as they are.
    /// Returns whether anything changed.
    fn apply_counts(&self, cx: &mut App) -> bool {
        let parts = self.parts().clone();
        let table = self.table();
        let profile = LaneProfiles::source(cx);
        let counts = effective_counts(&table, &self.0.config.project, profile.as_deref());
        if counts == *self.0.counts.borrow() {
            return false;
        }
        let lanes = table_lane_ids(&table, &self.0.config.project, profile.as_deref());
        let overlap = parts.host.check_locations(&table, &lanes, cx).err();
        let existing = match parts.writer.read(crate::store::read_state) {
            Ok(state) => state.lanes.into_keys().collect::<BTreeSet<_>>(),
            Err(error) => {
                log::error!("lanes: reading the lanes state for a count change: {error:#}");
                return false;
            }
        };
        {
            let mut overlapping = self.0.overlapping.borrow_mut();
            match &overlap {
                None => overlapping.clear(),
                Some(found) => {
                    let reason = problems(found);
                    overlapping.retain(|lane, _| lanes.contains(lane));
                    for lane in &lanes {
                        if !existing.contains(lane) || overlapping.contains_key(lane) {
                            overlapping.insert(lane.clone(), reason.clone());
                        }
                    }
                }
            }
        }
        let held = self.0.overlapping.borrow().clone();
        let now = parts.host.now();
        let result = parts
            .writer
            .transact(|tx| change_counts(tx, &counts, &held, &now));
        match result {
            Ok(()) => {
                *self.0.counts.borrow_mut() = counts;
                true
            }
            Err(error) => {
                log::error!("lanes: applying a count change: {error:#}");
                false
            }
        }
    }

    /// A lane command asks the lanes to look for work now.
    pub fn pick_now(&self, cx: &mut App) {
        self.evaluate(true, cx);
    }

    /// Runs a pass after the current one, or now when none runs.
    pub(crate) fn schedule(&self, full: bool, cx: &mut App) {
        let this = self.clone();
        cx.defer(move |cx| this.evaluate(full, cx));
    }

    /// One pass over the lanes. `full` also looks at lanes waiting on their environment. A pass
    /// asked for while one runs runs right after it.
    pub fn evaluate(&self, full: bool, cx: &mut App) {
        let inner = &self.0;
        if inner.ended.get() {
            return;
        }
        if inner.passing.get() {
            let pending = inner.again.get().unwrap_or(false);
            inner.again.set(Some(pending || full));
            return;
        }
        inner.passing.set(true);
        let mut full = full;
        loop {
            self.pass(full, cx);
            match inner.again.take() {
                Some(next) => full = next,
                None => break,
            }
        }
        inner.passing.set(false);
        if self.end_if_at_rest(cx) {
            inner.ended.set(true);
        } else if self.run_ended() {
            inner.ended.set(true);
        }
    }

    /// Whether the dispatcher's record says its run ended (another handle ended it).
    fn run_ended(&self) -> bool {
        self.parts()
            .writer
            .transact(dispatcher_record)
            .is_ok_and(|record| record.is_some_and(|record| record.ended.is_some()))
    }

    fn pass(&self, full: bool, cx: &mut App) {
        self.0.passes.set(self.0.passes.get() + 1);
        let writer = &self.parts().writer;
        if let Err(error) = writer.transact(|tx| self.forget_stopped_above_count(tx)) {
            log::error!("lanes: removing the stopped lanes above their count: {error:#}");
        }
        let lanes = match self.begin_pass(cx) {
            Ok(state) => {
                self.sweep_hooked(&state, cx);
                state.lanes.into_keys().collect::<Vec<_>>()
            }
            Err(error) => {
                log::error!("lanes: reading the lanes state: {error:#}");
                return;
            }
        };
        for lane in lanes {
            if let Err(error) = self.evaluate_lane(&lane, full, cx) {
                log::error!("lanes: evaluating {lane}: {error:#}");
            }
        }
    }

    fn evaluate_lane(&self, lane: &LaneId, full: bool, cx: &mut App) -> Result<()> {
        // A job in the job hooks keeps its lane until the hooks' `after_job` resolved.
        if self.0.hooked.borrow().contains_key(lane) {
            return Ok(());
        }
        let parts = self.parts().clone();
        let extensions = PickExtensions::extensions(cx);
        for _ in 0..MAX_RESTARTS {
            let master = self.0.verified.borrow().clone();
            let commit = parts.policy.commit_for_job(lane, &master);
            let generation = parts.writer.generation()?;
            let Some(graph) = parts.host.graph(cx) else {
                return Ok(());
            };
            let now = parts.host.now();
            let step = with_engine(&parts.engine, || {
                parts.writer.transact(|tx| {
                    self.lane_transaction(
                        tx,
                        lane,
                        full,
                        generation,
                        &graph,
                        &commit,
                        &now,
                        &extensions,
                    )
                })
            })?;
            match step {
                LaneStep::Restart => self.0.restarts.set(self.0.restarts.get() + 1),
                LaneStep::Nothing => return Ok(()),
                LaneStep::Start(job) => {
                    self.start_job(job, cx);
                    return Ok(());
                }
            }
        }
        log::warn!("lanes: {lane} not evaluated, the state kept moving");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn lane_transaction(
        &self,
        tx: &mut StateTx,
        lane: &LaneId,
        full: bool,
        generation: u64,
        graph: &PickGraph,
        commit: &JobCommit,
        now: &str,
        extensions: &[Arc<dyn PickExtension>],
    ) -> Result<LaneStep> {
        if tx.generation()? != generation {
            return Ok(LaneStep::Restart);
        }
        let state = read_state_in(tx)?;
        let Some(record) = state.lanes.get(lane) else {
            return Ok(LaneStep::Nothing);
        };
        let free = record.state == LaneState::Idle || (full && record.state == LaneState::Waiting);
        if !free || record.job.is_some() || !self.lane_exists(lane) {
            return Ok(LaneStep::Nothing);
        }
        if let Some(reason) = self.0.overlapping.borrow().get(lane) {
            let mut held = record.clone();
            held.state = LaneState::Waiting;
            held.reason = Some(WaitReason::Environment(reason.clone()));
            write_lane_if_changed(tx, record, &held)?;
            return Ok(LaneStep::Nothing);
        }
        let mut updated = record.clone();
        if updated.stop_requested.is_some() {
            updated.state = LaneState::Idle;
            self.stop_at_boundary(tx, &state, &updated, now)?;
            return Ok(LaneStep::Nothing);
        }
        if updated.pause_requested {
            updated.state = LaneState::Paused;
            updated.pause_requested = false;
            updated.reason = Some(paused_reason(&record.reason));
            put_lane(tx, &updated)?;
            return Ok(LaneStep::Nothing);
        }
        if let Some(max) = self.0.config.max_jobs {
            let base = *self
                .0
                .baseline
                .borrow_mut()
                .entry(lane.clone())
                .or_insert(record.jobs_done);
            if record.jobs_done.saturating_sub(base) >= max {
                self.stop_for_bound(tx, &state, updated, now)?;
                return Ok(LaneStep::Nothing);
            }
        }
        let commit = match commit {
            JobCommit::Use(commit) => commit.clone(),
            JobCommit::Wait(reason) => {
                updated.state = LaneState::Idle;
                updated.reason = Some(WaitReason::Other(reason.clone()));
                write_lane_if_changed(tx, record, &updated)?;
                return Ok(LaneStep::Nothing);
            }
            JobCommit::StopFirst(_) => {
                updated.state = LaneState::Idle;
                updated.stop_requested = Some(StopCause::Rollout);
                self.stop_at_boundary(tx, &state, &updated, now)?;
                return Ok(LaneStep::Nothing);
            }
        };

        let after_extensions;
        let state = if extensions.is_empty() {
            &state
        } else {
            match extension_pick(extensions, PickPoint::Before, tx, lane) {
                ExtensionPick::Pass => {}
                decided => return self.extension_decided(tx, lane, updated, decided, commit, now),
            }
            // An extension may have written in this transaction; the picker reads after it.
            after_extensions = read_state_in(tx)?;
            &after_extensions
        };
        let table = self.0.table.borrow();
        let options = self.pick_options(state, graph, &table);
        match pick(graph, state, &table, lane, &options) {
            PickOutcome::Assign(assignment) => {
                assign_job(tx, state, lane, updated, assignment, commit, now)
            }
            PickOutcome::Nothing {
                reasons,
                owner_change,
            } => {
                apply_owner_changes(tx, state, &owner_change, lane, now)?;
                if !extensions.is_empty() {
                    match extension_pick(extensions, PickPoint::After, tx, lane) {
                        ExtensionPick::Pass => {}
                        decided => {
                            return self.extension_decided(tx, lane, updated, decided, commit, now);
                        }
                    }
                }
                if self.0.config.bound.is_some() && only_bound(&reasons) {
                    self.stop_for_bound(tx, state, updated, now)?;
                    return Ok(LaneStep::Nothing);
                }
                updated.state = LaneState::Idle;
                updated.reason = Some(idle_reason(reasons));
                write_lane_if_changed(tx, record, &updated)?;
                Ok(LaneStep::Nothing)
            }
            PickOutcome::Hold(hold) => {
                updated.state = LaneState::Idle;
                updated.reason = Some(WaitReason::Held {
                    reason: format!("{}: {}", hold.task, hold.reason),
                });
                write_lane_if_changed(tx, record, &updated)?;
                Ok(LaneStep::Nothing)
            }
        }
    }

    /// Applies an extension's answer other than `Pass` to the free lane `updated`.
    fn extension_decided(
        &self,
        tx: &mut StateTx,
        lane: &LaneId,
        mut updated: LaneRecord,
        decided: ExtensionPick,
        commit: String,
        now: &str,
    ) -> Result<LaneStep> {
        // The extension may have written in this transaction.
        let state = read_state_in(tx)?;
        match decided {
            ExtensionPick::Assign(assignment) => {
                assign_job(tx, &state, lane, updated, assignment, commit, now)
            }
            ExtensionPick::StopLane(text) => {
                updated.state = LaneState::Idle;
                updated.stop_requested = Some(StopCause::Extension { text });
                self.stop_at_boundary(tx, &state, &updated, now)?;
                Ok(LaneStep::Nothing)
            }
            ExtensionPick::Pass => Ok(LaneStep::Nothing),
        }
    }

    /// Stops an idle lane that reached its bound, with the owner of its task.
    fn stop_for_bound(
        &self,
        tx: &mut StateTx,
        state: &LanesState,
        mut lane: LaneRecord,
        now: &str,
    ) -> Result<()> {
        lane.state = LaneState::Idle;
        lane.reason = Some(WaitReason::Other(BOUND_REASON.to_string()));
        lane.stop_requested = Some(StopCause::Bound);
        self.stop_at_boundary(tx, state, &lane, now)
    }

    /// Stops an idle lane with a stop asked for, with the owner of its task.
    fn stop_at_boundary(
        &self,
        tx: &mut StateTx,
        state: &LanesState,
        lane: &LaneRecord,
        now: &str,
    ) -> Result<()> {
        let held = held_task(state, &lane.id());
        let (lane, owner) = stop_at_job_boundary(lane, held.map(|(_, owner)| owner), now);
        put_lane(tx, &lane)?;
        if let (Some((task, _)), Some(owner)) = (held, owner) {
            put_owner(tx, task, &owner)?;
        }
        Ok(())
    }

    /// Removes the record of a lane above its count once it stopped, so a later raise makes it
    /// again; its task keeps its stopped owner, which any lane may take. Nothing on disk changes.
    fn forget_stopped_above_count(&self, tx: &mut StateTx) -> Result<()> {
        let state = read_state_in(tx)?;
        for (id, lane) in &state.lanes {
            if lane.state == LaneState::Stopped && !self.lane_exists(id) {
                delete_lane(tx, id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn lane_exists(&self, lane: &LaneId) -> bool {
        self.0
            .counts
            .borrow()
            .get(&lane.kind)
            .is_some_and(|count| lane.index <= *count)
    }

    /// The options of a pick: the effective counts, the bound, and the use of every workflow's
    /// instances, a lane job with a reserved run id that has not started counting as one.
    fn pick_options(
        &self,
        state: &LanesState,
        graph: &PickGraph,
        table: &LaneTable,
    ) -> PickOptions {
        let engine = &self.parts().engine;
        let mut workflows: BTreeSet<String> = table
            .rules
            .iter()
            .filter_map(|rule| {
                rule.workflow
                    .as_ref()
                    .map(|workflow| workflow.value.clone())
            })
            .collect();
        workflows.extend(
            graph
                .leaves()
                .filter_map(|leaf| leaf.open_todo.as_ref()?.workflow.clone()),
        );
        workflows.extend(
            state
                .lanes
                .values()
                .filter_map(|lane| lane.job.as_ref().map(|job| job.workflow.clone())),
        );
        let instances = workflows
            .into_iter()
            .map(|workflow| {
                let usage = instance_use(&**engine, state, &workflow);
                (workflow, usage)
            })
            .collect();
        PickOptions {
            counts: self.0.counts.borrow().clone(),
            instances,
            assigned: BTreeMap::new(),
            bound: self.0.config.bound.clone(),
            waiting_limit: None,
            stamp: self.0.stamp.borrow().clone(),
        }
    }

    /// The unattended choice of a job on `lane`: the start's, else the lane kind's, else the
    /// project's (unattended).
    pub(crate) fn job_unattended(&self, lane: &LaneId) -> UnattendedChoice {
        if let Some(choice) = self.0.config.unattended {
            return choice;
        }
        let kind = self
            .0
            .table
            .borrow()
            .kinds
            .iter()
            .find(|kind| kind.name == lane.kind)
            .and_then(|kind| kind.unattended);
        match kind {
            Some(value) => UnattendedChoice {
                value,
                source: UnattendedSource::Lane,
            },
            None => PROJECT_UNATTENDED,
        }
    }
}

/// The reason of an idle lane when the picker names no candidate at all.
pub const NOTHING_READY: &str = "nothing ready";

/// Whether every reason a lane found for waiting is the bound, another lane's ownership or a
/// finished task, with at least one the bound: the lane has reached its bound.
fn only_bound(reasons: &[bongos_lanes_model::Waiting]) -> bool {
    reasons
        .iter()
        .any(|waiting| waiting.reason == WaitReason::Bound)
        && reasons.iter().all(|waiting| {
            matches!(
                waiting.reason,
                WaitReason::Bound | WaitReason::Owner { .. } | WaitReason::Finished
            )
        })
}

/// What an idle lane shows: the best-ranked candidate's reason, skipping candidates another lane
/// owns (they are that lane's work, not this one's wait), or [`NOTHING_READY`].
fn idle_reason(mut reasons: Vec<bongos_lanes_model::Waiting>) -> WaitReason {
    if reasons.is_empty() {
        return WaitReason::Other(NOTHING_READY.to_string());
    }
    let index = reasons
        .iter()
        .position(|waiting| !matches!(waiting.reason, WaitReason::Owner { .. }))
        .unwrap_or(0);
    reasons.swap_remove(index).reason
}

/// The task `lane` holds, with its owner record: active, stopped, halted or ahead on it.
pub(crate) fn held_task<'a>(
    state: &'a LanesState,
    lane: &LaneId,
) -> Option<(&'a TaskId, &'a OwnerRecord)> {
    state.owners.iter().find(|(_, owner)| {
        owner.lane.as_ref() == Some(lane)
            && matches!(
                owner.state,
                OwnerState::Active | OwnerState::Stopped | OwnerState::Halted | OwnerState::Ahead
            )
    })
}

/// Applies changed counts in one transaction. Lanes up to a count that do not exist are made idle,
/// or waiting with the reason when `held` names them. Above a count: a lane with a job (running,
/// or waiting with a reserved job) stops after it; a free one stops now, its task stopped with it
/// so any lane may take it, and its record is removed; a halted one stays until a person acts. A
/// lane back within its count forgets a stop the count asked for.
fn change_counts(
    tx: &mut StateTx,
    counts: &BTreeMap<String, u32>,
    held: &BTreeMap<LaneId, String>,
    now: &str,
) -> Result<()> {
    let state = read_state_in(tx)?;
    let plan = reconcile_counts(counts, &state);
    for id in &plan.create {
        let mut lane = LaneRecord::new(id, LaneState::Idle);
        if let Some(reason) = held.get(id) {
            lane.state = LaneState::Waiting;
            lane.reason = Some(WaitReason::Environment(reason.clone()));
        }
        put_lane(tx, &lane)?;
    }
    for id in plan.remove_now.iter().chain(&plan.stop_after_job) {
        let Some(lane) = state.lanes.get(id) else {
            continue;
        };
        if lane.job.is_some() {
            let mut lane = lane.clone();
            lane.stop_requested = Some(StopCause::Count);
            put_lane(tx, &lane)?;
            continue;
        }
        if let Some((task, owner)) = held_task(&state, id) {
            let stopped = owner_after_stop(owner, StopCause::Count, now);
            if stopped != *owner {
                put_owner(tx, task, &stopped)?;
            }
        }
        delete_lane(tx, id)?;
    }
    for (id, lane) in &state.lanes {
        let within = counts.get(&id.kind).is_some_and(|count| id.index <= *count);
        if within && lane.stop_requested == Some(StopCause::Count) {
            let mut lane = lane.clone();
            lane.stop_requested = None;
            put_lane(tx, &lane)?;
        }
    }
    Ok(())
}

fn write_lane_if_changed(tx: &mut StateTx, before: &LaneRecord, after: &LaneRecord) -> Result<()> {
    if before != after {
        put_lane(tx, after)?;
    }
    Ok(())
}

/// Applies the owner changes of a pick, by `lane`.
fn apply_owner_changes(
    tx: &mut StateTx,
    state: &LanesState,
    changes: &[OwnerChange],
    lane: &LaneId,
    now: &str,
) -> Result<()> {
    for change in changes {
        let owner = match state.owners.get(&change.task) {
            Some(existing) => {
                let mut owner = existing.clone();
                if owner.state != change.to {
                    owner.since = now.to_string();
                    owner.reason = None;
                }
                owner.state = change.to;
                owner.lane = change.lane.clone();
                owner.by = Author::Lane(lane.clone());
                owner
            }
            None => OwnerRecord {
                lane: change.lane.clone(),
                state: change.to,
                since: now.to_string(),
                by: Author::Lane(lane.clone()),
                reason: None,
                prepared_commit: None,
                refreshed: false,
            },
        };
        if state.owners.get(&change.task) != Some(&owner) {
            put_owner(tx, &change.task, &owner)?;
        }
    }
    Ok(())
}

/// Makes `assignment` the job of the free lane `updated` and reserves its run id, the
/// assignment's own when an extension gave one.
fn assign_job(
    tx: &mut StateTx,
    state: &LanesState,
    lane: &LaneId,
    mut updated: LaneRecord,
    assignment: Assignment,
    commit: String,
    now: &str,
) -> Result<LaneStep> {
    apply_owner_changes(tx, state, &assignment.owner_change, lane, now)?;
    let extension = match &assignment.work {
        AssignedWork::Extension { kind, .. } => Some(kind.clone()),
        AssignedWork::Leaf { .. } => None,
    };
    let job = JobRecord {
        run_id: assignment
            .run_id
            .clone()
            .unwrap_or_else(bongos_work_model::new_id),
        subject: job_subject(&assignment.work),
        workflow: assignment.workflow.clone(),
        started: now.to_string(),
        definition_commit: Some(commit),
        extension,
    };
    updated.state = LaneState::Running;
    updated.reason = None;
    updated.job = Some(job.clone());
    put_lane(tx, &updated)?;
    Ok(LaneStep::Start(PendingJob {
        lane: lane.clone(),
        job,
        work: assignment.work,
        inputs: assignment.inputs,
    }))
}

fn job_subject(work: &AssignedWork) -> RunSubject {
    match work {
        AssignedWork::Leaf { task, .. } => RunSubject::Task(task.clone()),
        AssignedWork::Extension {
            task: Some(task), ..
        } => RunSubject::Task(task.clone()),
        AssignedWork::Extension {
            kind, task: None, ..
        } => RunSubject::Other(kind.clone()),
    }
}

/// Whether a start refusal is the machine's (the lane waits) rather than the job's (the lane
/// halts).
pub(crate) fn environment_refusal(kind: RefusalKind) -> bool {
    matches!(
        kind,
        RefusalKind::AgentUnavailable
            | RefusalKind::InterpreterMissing
            | RefusalKind::McpServerMissing
            | RefusalKind::LowDiskSpace
            | RefusalKind::NoTaskWorktree
    )
}
