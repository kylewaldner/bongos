//! The lanes in the process that holds a project's writer, and the `lanes:` commands it serves
//! over the engine host's socket. A window that holds the writer serves the commands of another
//! window or of the command line; a person's claim in a command is trusted only from a window of
//! this executable (the peer's identity says so), and from any other peer the command is recorded
//! as the system's.
//!
//! Starting the dispatcher and the commands that need it (`start`, `pick`) belong to the engine
//! host process, not to a window: a window that holds the writer hands it over to an engine host
//! on `lanes: start` (see `actions.rs`), and answers another process's `start` that they run
//! there. The engine host keeps the dispatcher it started in [`HostDispatcher`] until its run
//! ends.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use anyhow::{Result, anyhow, bail};
use bongos_engine_host::{EngineHostProcess, HostCommands, HostEvent, LanesVerb, WriterHosts};
use bongos_lanes_model::{
    Author, JobRecord, LaneId, LaneTable, LiveFacts, PickBound, PickGraph, Problem, RunSubject,
    StampFile, TaskId, from_work_tree, parse, short_run,
};
use bongos_project_repo::{
    ChangeFilter, ChangeState, LandingEvent, LandingResult, PeerIdentity, PersonClaim, ProjectRepo,
    SnapshotReason,
};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use bongos_work::{OpenProjectRepos, WorkStore};
use bongos_workflow::worktrees::{Git, run_branch_name};
use bongos_workflow::{RunStatus, WorkflowEngine};
use chrono::Utc;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Global, Task};
use serde::{Deserialize, Serialize};
use serde_json::json;
use settings::Settings as _;

use crate::LanesSettings;
use crate::accept_graph::{accept_graph_for_peer, file_at, read_stamp};
use crate::actions::{OFF_MESSAGE, WindowLanes, accepted_message, adopted_message};
use crate::adopt::{AdoptRequest, LeftWork};
use crate::definitions::{LANES_FILE, LANES_FOLDER};
use crate::dispatcher::{
    AheadLanding, Dispatcher, JobEngine, Lanes, LanesHost, LanesParts, StartLanes,
    VerifiedCommitPolicy,
};
use crate::job_trees::forbidden_for;
use crate::lane_control::{LaneTarget, LanesStatus};
use crate::lane_tree::{LastEnd, LastRun, ResetRequest};
use crate::locations::{LocationInputs, check_lanes};
use crate::profile::LaneProfiles;
use crate::staleness::PinnedTree;

/// What a `lanes:` command carries besides its verb.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanesPayload {
    /// The task to adopt.
    pub task: Option<String>,
    /// The lane a command acts on (`build-2`); `None` is every lane for a stop.
    pub lane: Option<String>,
    /// Why an adopt.
    pub reason: Option<String>,
    /// The person agreed to drop a halted job's unreadable work.
    #[serde(default)]
    pub confirm_drop: bool,
    /// A start bounded to this group (`task: run` on a group): the lanes take only leaves under it.
    #[serde(default)]
    pub within: Option<String>,
    /// The person at the sending window's keyboard. The writer takes it as the author only from a
    /// window of its own executable; a command-line peer's is ignored.
    pub person: Option<String>,
}

/// The message the host answers a command with.
fn message(text: impl Into<String>) -> serde_json::Value {
    json!({ "message": text.into() })
}

/// Serves the `lanes:` commands in this process's writer.
pub fn register_host(cx: &mut App) {
    HostCommands::serve_lanes(
        Rc::new(|verb, payload, peer, cx| {
            if EngineHostProcess::is_running(cx) {
                log::info!("lanes {verb:?} from pid {}", peer.pid());
            }
            let answered = handle(verb, payload, peer, cx);
            if answered.is_ok() {
                WriterHosts::broadcast(&HostEvent::LanesChanged, cx);
            }
            match answered {
                Ok(task) => task,
                Err(error) => Task::ready(Err(error)),
            }
        }),
        cx,
    );
}

/// The author of a command: the person it names when `peer` may attest one, else the system.
pub(crate) fn author_of(payload: &LanesPayload, peer: &PeerIdentity) -> Author {
    match (&payload.person, peer.may_attest_person()) {
        (Some(name), true) => Author::Person(name.clone()),
        _ => Author::System,
    }
}

fn handle(
    verb: LanesVerb,
    payload: serde_json::Value,
    peer: &PeerIdentity,
    cx: &mut App,
) -> Result<Task<Result<serde_json::Value>>> {
    if !LanesSettings::get_global(cx).enabled {
        bail!(OFF_MESSAGE);
    }
    let payload: LanesPayload = serde_json::from_value(payload)
        .map_err(|error| anyhow!("reading the lanes command: {error}"))?;
    let by = author_of(&payload, peer);
    if verb == LanesVerb::AcceptGraph {
        let repo = sole_writer_repo(cx)?;
        let prefix = bongos_project_repo::ProjectRepositorySettings::get_global(cx)
            .branch_prefix
            .clone();
        repo.use_registered_checks(cx);
        let claim = payload.person.map(|name| PersonClaim { name });
        let peer = peer.clone();
        return Ok(cx.background_spawn(async move {
            let accepted = accept_graph_for_peer(&repo, claim, &peer, &prefix, Utc::now()).await;
            Ok(message(accepted_message(&accepted)))
        }));
    }
    let lanes = ensure_window_lanes(cx).ok_or_else(|| {
        anyhow!("lanes need exactly one project repository open here, with its writer")
    })?;
    match verb {
        LanesVerb::Adopt => {
            let request = AdoptRequest {
                task: TaskId::new(
                    payload
                        .task
                        .ok_or_else(|| anyhow!("name the task to adopt"))?,
                ),
                lane: parse_lane(payload.lane.as_deref())?,
                reason: payload.reason.unwrap_or_default(),
                by,
                confirm_drop: payload.confirm_drop,
            };
            let adopting = lanes.adopt(request, cx);
            Ok(cx.spawn(async move |_| {
                let result = adopting.await;
                match &result {
                    Err(error)
                        if matches!(
                            error.downcast_ref::<bongos_lanes_model::AdoptRefusal>(),
                            Some(bongos_lanes_model::AdoptRefusal::UnreadableTree(_))
                        ) =>
                    {
                        Ok(json!({ "unreadable": adopted_message(&result) }))
                    }
                    Err(error) => Err(anyhow!("{error:#}")),
                    Ok(_) => Ok(message(adopted_message(&result))),
                }
            }))
        }
        LanesVerb::Stop => {
            let target = match payload.lane.as_deref() {
                Some(lane) => LaneTarget::Lane(parse_lane(Some(lane))?),
                None => LaneTarget::All,
            };
            lanes.stop(target, by, cx)?;
            Ok(Task::ready(Ok(message("Stopping at the job boundary"))))
        }
        LanesVerb::Pause => {
            let text = lanes.pause(&parse_lane(payload.lane.as_deref())?, &by)?;
            Ok(Task::ready(Ok(message(text))))
        }
        LanesVerb::Resume => {
            let text = lanes.resume(&parse_lane(payload.lane.as_deref())?)?;
            Ok(Task::ready(Ok(message(text))))
        }
        LanesVerb::Status => {
            let status = lanes.status()?;
            let doc = status_document(&status, cx);
            Ok(Task::ready(Ok(json!({
                "message": status_text(&status),
                "status": doc,
            }))))
        }
        LanesVerb::Start if EngineHostProcess::is_running(cx) => {
            let starting = lanes.start(
                StartLanes {
                    bound: payload
                        .within
                        .map(|group| PickBound::Within(TaskId::new(group))),
                    max_jobs: None,
                    unattended: None,
                    by,
                },
                cx,
            );
            Ok(cx.spawn(async move |cx| {
                let started = starting.await?;
                let run = started.run_id.clone();
                if let Some(dispatcher) = started.dispatcher {
                    log::info!("lanes started: dispatcher run {run}");
                    cx.update(|cx| cx.set_global(HostDispatcher(Some(dispatcher))));
                    Ok(message(format!("Lanes started (run {run})")))
                } else {
                    Ok(message(started_message(&started)))
                }
            }))
        }
        LanesVerb::Pick if EngineHostProcess::is_running(cx) => {
            let dispatcher = running_dispatcher(cx)
                .ok_or_else(|| anyhow!("lanes are not started here; start them first"))?;
            dispatcher.pick_now(cx);
            Ok(Task::ready(Ok(message("Looking for work now"))))
        }
        LanesVerb::Start | LanesVerb::Pick => Ok(Task::ready(Err(anyhow!(
            "starting lanes and `pick` run in the engine host process, which this window does \
             not start"
        )))),
        LanesVerb::DryRun => Ok(Task::ready(Err(anyhow!(
            "`dry run` is not served by the engine host of this build yet"
        )))),
        LanesVerb::AcceptGraph => unreachable!("handled above"),
    }
}

/// What a start answers: it started, or lanes already run and any bound the start asked for was
/// not applied to the run in force.
pub(crate) fn started_message(started: &crate::LanesStarted) -> String {
    let run = &started.run_id;
    if started.dispatcher.is_some() {
        return format!("Lanes started (run {run})");
    }
    match &started.unapplied_bound {
        Some(PickBound::Within(group)) => format!(
            "Lanes already run (run {run}); the bound within {} was not applied, the run keeps \
             the bound it started with",
            group.0
        ),
        Some(PickBound::Until(task)) => format!(
            "Lanes already run (run {run}); the bound until {} was not applied, the run keeps \
             the bound it started with",
            task.0
        ),
        None => format!("Lanes already run (run {run})"),
    }
}

fn parse_lane(text: Option<&str>) -> Result<LaneId> {
    let text = text.ok_or_else(|| anyhow!("name a lane"))?;
    LaneId::parse(text).ok_or_else(|| anyhow!("{text} is not a lane (like build-2)"))
}

/// The `lanes-status 1` document of this process's project.
fn status_document(status: &LanesStatus, cx: &App) -> serde_json::Value {
    let repo = sole_writer_repo(cx).ok();
    let split = repo
        .as_ref()
        .and_then(|repo| repo.split_master().ok().flatten());
    let project = repo
        .as_ref()
        .map(|repo| repo.location().id.clone())
        .unwrap_or_default();
    let graph = WorkStore::global(cx)
        .and_then(|store| store.read(cx).snapshot())
        .and_then(|tree| from_work_tree(&tree, &project, &LiveFacts::default()));
    let engine = cx.try_global::<WorkflowEngine>().cloned();
    crate::status_doc::status_doc_with(
        status,
        &crate::status_doc::StatusFacts {
            project: &project,
            split: split.as_ref(),
            goal: &|task| {
                graph
                    .as_ref()
                    .and_then(|graph| graph.task(task))
                    .map(|task| task.name.clone())
            },
            run_lifecycle: &|run| {
                engine
                    .as_ref()
                    .and_then(|engine| engine.lifecycle().status(run))
                    .map(|status| format!("{status:?}").to_lowercase())
            },
            // A running job of a lane waits on the landing while master is split; the
            // landing's own wait event is not tracked per lane here.
            waits_on_landing: &|_| true,
        },
        &crate::wait_text::WaitTextFormatters::global(cx),
    )
}

fn status_text(status: &LanesStatus) -> String {
    if status.lanes.is_empty() {
        return "No lanes".to_string();
    }
    status
        .lanes
        .iter()
        .map(|lane| {
            let line = match &lane.task {
                Some(task) => format!("{}: {:?} on {task}", lane.id, lane.state),
                None => format!("{}: {:?}", lane.id, lane.state),
            };
            match &lane.asked {
                Some(text) => format!("{line}, needs you: {text}"),
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The dispatcher the engine host process started, kept until its run ends.
#[derive(Default)]
pub struct HostDispatcher(Option<Dispatcher>);

impl Global for HostDispatcher {}

/// The dispatcher this process runs, while its run has not ended.
fn running_dispatcher(cx: &App) -> Option<Dispatcher> {
    cx.try_global::<HostDispatcher>()?
        .0
        .clone()
        .filter(|dispatcher| !dispatcher.has_ended())
}

/// Whether this process runs the lanes' dispatcher now (the engine host is busy while it does).
pub fn dispatcher_running(cx: &App) -> bool {
    running_dispatcher(cx).is_some()
}

/// The only project repository open here, which this process must hold the writer of.
fn sole_writer_repo(cx: &App) -> Result<ProjectRepo> {
    let repos = OpenProjectRepos::listed(cx);
    let [project] = repos.as_slice() else {
        bail!("open exactly one project repository to use lanes");
    };
    let repo = project.repo().clone();
    if repo.state().and_then(|state| state.writer()).is_none() {
        bail!("this process does not hold the project's writer");
    }
    Ok(repo)
}

/// The lanes of the only project this process holds the writer of, made on first use and kept in
/// [`WindowLanes`]. `None` with no such project or while workflows are off.
pub fn ensure_window_lanes(cx: &mut App) -> Option<Rc<Lanes>> {
    if let Some(lanes) = cx
        .try_global::<WindowLanes>()
        .and_then(|window_lanes| window_lanes.0.clone())
    {
        return Some(lanes);
    }
    let repo = sole_writer_repo(cx).ok()?;
    let writer = repo.state()?.writer()?.clone();
    let engine = WorkflowEngine::ensure(cx)?;
    let mut project_host = ProjectLanesHost::new(repo);
    if EngineHostProcess::is_running(cx) {
        project_host.id_prefix = HOST_PREFIX;
    }
    let host: Rc<dyn LanesHost> = Rc::new(project_host);
    let engine: Rc<dyn JobEngine> = Rc::new(engine);
    let lanes = Rc::new(Lanes::new(LanesParts {
        host,
        engine,
        policy: Rc::new(VerifiedCommitPolicy),
        writer,
    }));
    cx.set_global(WindowLanes(Some(lanes.clone())));
    Some(lanes)
}

/// What the app tells the lanes of one project repository: its table and stamp at a commit, its
/// task graph, its locations, and the work a halted job left in its lane's trees.
pub struct ProjectLanesHost {
    pub repo: ProjectRepo,
    /// The lane table and stamp text at the commits [`LanesHost::load_commit`] read, so the
    /// sync calls never run git on the main thread.
    loaded: Rc<RefCell<HashMap<String, AtCommit>>>,
    /// How [`LanesHost::host_id`] names this process: [`WINDOW_PREFIX`] or [`HOST_PREFIX`].
    pub id_prefix: &'static str,
}

/// The prefix of a window's host id.
pub const WINDOW_PREFIX: &str = "window";
/// The prefix of an engine host process's host id.
pub const HOST_PREFIX: &str = "host";

/// What git said about a commit's lane files.
#[derive(Clone)]
struct AtCommit {
    table: Result<Option<String>, String>,
    stamp: Result<Option<StampFile>, String>,
}

/// How many commits' files are kept; the verified commit only moves forward.
const LOADED_COMMITS: usize = 4;

fn not_loaded(commit: &str) -> String {
    format!("the lane files at {commit} were not read yet")
}

fn problem(message: String) -> Vec<Problem> {
    vec![Problem {
        requirement: "the lane locations can be resolved".to_string(),
        message,
        hint: None,
    }]
}

impl ProjectLanesHost {
    pub fn new(repo: ProjectRepo) -> Self {
        Self {
            repo,
            loaded: Rc::default(),
            id_prefix: WINDOW_PREFIX,
        }
    }

    fn table_path() -> String {
        format!("{DEFINITIONS_DIR}/{LANES_FOLDER}/{LANES_FILE}")
    }

    /// The task's repository checkout and branch, from the work tree.
    fn task_repository(
        &self,
        job: &JobRecord,
        inputs: &LocationInputs,
        cx: &App,
    ) -> Option<(String, std::path::PathBuf, String)> {
        let RunSubject::Task(task_id) = &job.subject else {
            return None;
        };
        let tree = WorkStore::global(cx)?.read(cx).snapshot()?;
        let file = tree.task_by_id(&task_id.0)?.file.clone()?;
        let branch = file.branch?;
        let name = file.repository?;
        let place = inputs
            .repositories
            .iter()
            .find(|place| place.name == name)?;
        Some((name, place.path.clone(), branch))
    }
}

impl LanesHost for ProjectLanesHost {
    fn project_id(&self) -> String {
        self.repo.location().id.clone()
    }

    fn is_repository_project(&self) -> bool {
        true
    }

    fn load_commit(&self, commit: &str, cx: &mut App) -> Task<()> {
        if self.loaded.borrow().contains_key(commit) {
            return Task::ready(());
        }
        let repo = self.repo.clone();
        let commit = commit.to_string();
        let loaded = self.loaded.clone();
        let reading = cx.background_spawn({
            let commit = commit.clone();
            async move {
                let table = file_at(&repo, &commit, &Self::table_path())
                    .await
                    .map_err(|error| format!("{error:#}"));
                let stamp = read_stamp(&repo, &commit)
                    .await
                    .map_err(|error| format!("{error:#}"));
                AtCommit { table, stamp }
            }
        });
        cx.spawn(async move |_| {
            let read = reading.await;
            let mut loaded = loaded.borrow_mut();
            if loaded.len() >= LOADED_COMMITS {
                loaded.clear();
            }
            loaded.insert(commit, read);
        })
    }

    fn lane_table(&self, commit: &str) -> Result<LaneTable, String> {
        let text = self
            .loaded
            .borrow()
            .get(commit)
            .ok_or_else(|| not_loaded(commit))?
            .table
            .clone()?
            .unwrap_or_else(|| "schema = 1\n".to_string());
        parse(&text).map_err(|error| format!("line {}: {}", error.line, error.message))
    }

    fn check_locations(
        &self,
        table: &LaneTable,
        lanes: &[LaneId],
        cx: &App,
    ) -> Result<(), Vec<Problem>> {
        let inputs = LocationInputs::from_app(&self.project_id(), cx).map_err(problem)?;
        let profile = LaneProfiles::source(cx);
        check_lanes(&inputs, table, lanes, profile.as_deref()).map(|_| ())
    }

    fn graph(&self, cx: &App) -> Option<PickGraph> {
        let tree = WorkStore::global(cx)?.read(cx).snapshot()?;
        from_work_tree(&tree, &self.project_id(), &LiveFacts::default())
    }

    /// `window-<pid>-<start time>` (`host-` in an engine host process), or `window-<pid>` where
    /// the system reports no start time.
    fn host_id(&self) -> String {
        let pid = std::process::id();
        let prefix = self.id_prefix;
        match bongos_workflow::process::start_time(pid) {
            Some(start) => format!("{prefix}-{pid}-{start}"),
            None => format!("{prefix}-{pid}"),
        }
    }

    /// The same rule as `host.json`: the pid runs now with the start time the id recorded.
    fn host_live(&self, host: &str) -> bool {
        let Some(rest) = [WINDOW_PREFIX, HOST_PREFIX]
            .iter()
            .find_map(|prefix| host.strip_prefix(prefix)?.strip_prefix('-'))
        else {
            return false;
        };
        let (pid, start) = match rest.split_once('-') {
            Some((pid, start)) => (pid, start.parse::<u64>().ok()),
            None => (rest, None),
        };
        pid.parse::<u32>()
            .is_ok_and(|pid| bongos_engine_host::process_is(pid, start))
    }

    fn now(&self) -> String {
        Utc::now().to_rfc3339()
    }

    fn left_work(&self, lane: &LaneId, job: &JobRecord, cx: &App) -> Task<Vec<LeftWork>> {
        let Some(mut work) = self.left_work_without_branch(lane, job, cx) else {
            return Task::ready(Vec::new());
        };
        let git = self.repo.git().clone();
        let run = job.run_id.clone();
        cx.background_spawn(async move {
            let request = &mut work.request;
            if let Some(last) = request.last.as_mut() {
                last.run_branch =
                    latest_run_branch(&git, &request.repository, &request.add_from, &run).await;
            }
            vec![work]
        })
    }

    fn stamp(&self, commit: &str) -> Result<Option<StampFile>, String> {
        self.loaded
            .borrow()
            .get(commit)
            .ok_or_else(|| not_loaded(commit))?
            .stamp
            .clone()
    }

    fn materialize(&self, commit: Option<&str>, cx: &mut App) -> Task<Result<PinnedTree, String>> {
        let commit = match commit {
            Some(commit) => commit.to_string(),
            None => match self.verified_commit() {
                Some(commit) => commit,
                None => return Task::ready(Err("the project has no verified commit".to_string())),
            },
        };
        let repo = self.repo.clone();
        cx.background_spawn(async move {
            let guard = repo
                .materialize(&commit)
                .await
                .map_err(|error| format!("{error:#}"))?;
            Ok(PinnedTree {
                root: guard.path().to_path_buf(),
                guard: Box::new(guard),
            })
        })
    }

    fn ahead_landing(&self, run: &str, cx: &mut App) -> Task<AheadLanding> {
        // Subscribed before the changes are read, so no landing that ends meanwhile is missed.
        let events = self.repo.landing_events();
        // The open and requested changes. An open one counts while this process lands it, or
        // is about to: a quick edit's row is open until its landing, which runs on after the
        // run heard that it started, takes it.
        let filter = ChangeFilter {
            run: Some(run.to_string()),
            state: None,
            ..ChangeFilter::default()
        };
        let pending = match self.repo.changes(&filter) {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| {
                    row.state == ChangeState::Requested
                        || self.repo.landing_in_process(&row.change_id)
                })
                .map(|row| row.change_id)
                .collect(),
            Err(error) => {
                return Task::ready(AheadLanding::Refused(format!(
                    "reading the changes of run {}: {error:#}",
                    short_run(run)
                )));
            }
        };
        let repo = self.repo.clone();
        cx.background_spawn(async move { await_landings(&repo, pending, events).await })
    }

    /// The run engine snapshots the other job ends when it exports the run; a halted run it
    /// does not, so it is taken here.
    fn snapshot_after_job(&self, _lane: &LaneId, run: &str, status: RunStatus, cx: &mut App) {
        if status != RunStatus::Halted {
            return;
        }
        let repo = self.repo.clone();
        let run = run.to_string();
        cx.background_spawn(async move {
            if let Err(error) = repo.snapshot_now(SnapshotReason::JobEnd, Utc::now()).await {
                log::warn!("lanes: snapshot after halted run {run}: {error:#}");
            }
        })
        .detach();
    }
}

/// What one landing event of a change an ahead run waits on says: `None` while it is landing.
fn landing_end(event: LandingEvent) -> Option<Result<String, String>> {
    match event {
        LandingEvent::Landed { commit } => Some(Ok(commit)),
        LandingEvent::Refused { reasons } => Some(Err(reasons.join("; "))),
        LandingEvent::Conflict { files } => Some(Err(format!("conflict in {}", files.join(", ")))),
        LandingEvent::Ended { reason } => Some(Err(reason)),
        LandingEvent::Queued { .. } | LandingEvent::Checking | LandingEvent::Waiting { .. } => None,
    }
}

/// The reason an ahead job halts when a change it asked to land was left requested by an earlier
/// process: nothing in this one lands it, so no landing would ever report its end.
pub(crate) const LANDING_INTERRUPTED: &str =
    "its landing was interrupted by a restart; land it again, then adopt the lane";

/// Waits until every change in `pending` landed (the verified commit after them, or nothing when
/// there was none to wait for) or one did not (refused, in conflict, ended with no verdict,
/// abandoned, or left requested by an earlier process).
pub(crate) async fn await_landings(
    repo: &ProjectRepo,
    mut pending: BTreeSet<String>,
    mut events: futures::channel::mpsc::UnboundedReceiver<(String, LandingEvent)>,
) -> AheadLanding {
    let mut last = None;
    // A landing that ended before the subscription: its row, or the last event it announced.
    for change in pending.clone() {
        // Asked first: a landing that ends after this shows in the row or its last event.
        let in_process = repo.landing_in_process(&change);
        let ended = match repo.landing_result(&change) {
            Ok(Some(LandingResult::Landed { commit })) => Some(Ok(commit)),
            Ok(Some(LandingResult::Abandoned)) => {
                Some(Err(format!("change {change} was abandoned")))
            }
            Ok(_) => repo
                .landing_status(&change)
                .and_then(landing_end)
                .or_else(|| {
                    (!in_process).then(|| Err(format!("change {change}: {LANDING_INTERRUPTED}")))
                }),
            Err(error) => Some(Err(format!("{error:#}"))),
        };
        match ended {
            Some(Ok(commit)) => {
                pending.remove(&change);
                last = Some(commit);
            }
            Some(Err(reason)) => return AheadLanding::Refused(reason),
            None => {}
        }
    }
    while !pending.is_empty() {
        let Some((change, event)) = events.next().await else {
            return AheadLanding::Refused("the landing engine stopped".to_string());
        };
        if !pending.contains(&change) {
            continue;
        }
        match landing_end(event) {
            Some(Ok(commit)) => {
                pending.remove(&change);
                last = Some(commit);
            }
            Some(Err(reason)) => return AheadLanding::Refused(reason),
            None => {}
        }
    }
    // Prepared at master as it is now, which holds every landed change, whatever order they
    // landed in; the last landed commit only when the state cannot say.
    let Some(last) = last else {
        return AheadLanding::Nothing;
    };
    let verified = repo
        .state()
        .and_then(|state| state.read(|read| read.verified_commit()).ok().flatten());
    AheadLanding::Landed {
        commit: verified.unwrap_or(last),
    }
}

impl ProjectLanesHost {
    /// The verified commit the project's state records.
    fn verified_commit(&self) -> Option<String> {
        self.repo
            .state()
            .and_then(|state| state.read(|read| read.verified_commit()).ok().flatten())
    }

    /// The work `job` left on `lane`, its last run branch not looked up yet (that needs git).
    fn left_work_without_branch(
        &self,
        lane: &LaneId,
        job: &JobRecord,
        cx: &App,
    ) -> Option<LeftWork> {
        let inputs = LocationInputs::from_app(&self.project_id(), cx).ok()?;
        let (repo_name, repository, branch) = self.task_repository(job, &inputs, cx)?;
        let verified = self.verified_commit()?;
        let table = self.lane_table(&verified).ok()?;
        let profile = LaneProfiles::source(cx);
        let ids = crate::locations::table_lane_ids(&table, &self.project_id(), profile.as_deref());
        let all = check_lanes(&inputs, &table, &ids, profile.as_deref()).ok()?;
        let locations = all
            .iter()
            .find(|locations| &locations.lane == lane)
            .cloned()?;
        let tree = WorkStore::global(cx)
            .and_then(|store| store.read(cx).snapshot())
            .map(|tree| (*tree).clone())
            .unwrap_or_default();
        let forbidden = forbidden_for(lane, &inputs, &all, &tree);
        Some(LeftWork {
            git: Git::new("git"),
            request: ResetRequest {
                locations,
                repo: repo_name,
                repository,
                second: false,
                add_from: branch,
                start: String::new(),
                run_branch: String::new(),
                last: Some(LastRun {
                    run_id: job.run_id.clone(),
                    run_branch: String::new(),
                    end: LastEnd::Halted,
                }),
                save_max_file_bytes: LanesSettings::get_global(cx).save_max_file_bytes,
                forbidden,
            },
        })
    }
}

/// The run branch of `run` in `repository` with the most attempts: `run_branch_name` with the
/// attempt the run reached, which only git knows.
async fn latest_run_branch(
    git: &bongos_project_repo::Git,
    repository: &std::path::Path,
    task_branch: &str,
    run: &str,
) -> String {
    let first = run_branch_name(task_branch, run, 1);
    let prefix = first.trim_end_matches('1').to_string();
    let pattern = format!("refs/heads/{prefix}*");
    let listed = git
        .run(
            repository,
            &["for-each-ref", "--format=%(refname:short)", &pattern],
        )
        .await;
    let Ok(listed) = listed else {
        return first;
    };
    listed
        .lines()
        .filter_map(|branch| {
            let attempt: u32 = branch.strip_prefix(&prefix)?.parse().ok()?;
            Some((attempt, branch.to_string()))
        })
        .max()
        .map_or(first, |(_, branch)| branch)
}
