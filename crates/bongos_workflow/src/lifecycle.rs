//! The lifecycle of runs (feature 20): start with its refusals in order, pause, halt,
//! resume, retry step (fresh or not), restart, cancel, approve and revise, and the resume command
//! of a node's session.
//!
//! [`Lifecycle`] is plain Rust over the [`Runtime`]: it knows the run records (`run.toml`, the
//! attempt directories and their definition copies) and the run worktrees, and turns each command
//! into runtime calls. What the app knows (the work tree, the run another source reports, the
//! agent health) comes in a [`StartContext`], computed before the lifecycle is locked, so a source
//! that reads the lifecycle is never asked while it is busy. Every change of a run's status is
//! queued as a [`WorkflowEvent`] for the app to hand on.
//!
//! The work model stays free of runs: the lifecycle reads the tree and writes only under the
//! task's `runs/` directory, or for a run with no task under its project's.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_confine::safe_git::linked_git_dir_from_record;
use bongos_project_repo::CommitId;
use bongos_work::ActiveRun;
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{TodoState, new_id, now};
use bongos_workflow_model::definition::{AgentKind, Node, NodeKind};
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::{
    Attempt, EndState, RUN_SCHEMA, RecordedInput, StartedBy, TodoRef, is_uuid_v7,
};
use bongos_workflow_model::{
    Definition, EnvEdit, InputValue, LaneRef, Layer, LayerDirs, NodeKey, RunState, RunSubject,
    RunToml, RunTreeRecord, UnattendedChoice, ValidationContext, check_fresh_retry, parse,
    project_subject_problems, retry_set, schema,
};
use serde_json::Value;

use crate::WorkflowsSettings;
use crate::builtin_merge::{
    ProjectMergeRefusal, ProjectMergeRequest, ProjectMerged, merge_project_run,
};
use crate::definitions::{self, DefinitionError};
use crate::mcp;
use crate::runtime::{
    ProjectSource, RunBinding, RunSetup, RunSpec, Runner, Runtime, RuntimeError, Settlement,
};
use crate::worktrees::{self, Git, RunWorktree, TaskCheckout, WorktreeError, branch_worktree_path};

/// A run's id: a UUID v7, the same for every attempt.
pub type RunId = String;

/// The directory of a task holding its runs (03 reserves it).
pub const RUNS_DIR: &str = "runs";
/// The record of a run in its directory.
pub const RUN_FILE: &str = "run.toml";
/// Who closes a gate from the app.
pub const BY_PERSON: &str = "person";
/// The heading a revision comment goes under in the node's prompt.
pub const REVISION_HEADING: &str = "Revision requested:";

/// What a start runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartSubject {
    /// A task, completing its todo at `todo_index`; `None` is the task's current todo (the first
    /// open one).
    Task {
        id: String,
        todo_index: Option<usize>,
    },
    /// A whole project: a run with no task and no todo.
    Project { id: String },
}

impl StartSubject {
    /// The task id, `None` for a project subject.
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::Task { id, .. } => Some(id),
            Self::Project { .. } => None,
        }
    }
}

/// The tree a run works in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTree {
    /// A new run worktree. For a task run every field is `None`: the worktree is made from the
    /// task's own repository and branch, as it always was. For a project run, a worktree of the
    /// repository `repo` on a new run branch from `base` (by default the branch its main
    /// checkout has out); the success end merges the run branch into `merge_into` when set.
    Fresh {
        repo: Option<PathBuf>,
        base: Option<String>,
        merge_into: Option<String>,
    },
    /// A lane's tree at `path`. `common_dir` is the repository's git common directory, read in
    /// the person's own checkout (git never runs by discovery in the lane's tree, which jobs
    /// wrote). `base` is the second tree's path, `start` the ref the run branch is created from
    /// and `merge_into` the branch the success merge targets.
    Lane {
        path: PathBuf,
        common_dir: Option<PathBuf>,
        base: Option<PathBuf>,
        start: Option<String>,
        merge_into: Option<String>,
    },
    /// A tree materialized at `path` that the run reads and never writes: `BONGOS_WORKTREE` is
    /// `path`, its programs may write only their scratch, and there is no run branch or merge.
    /// Only a project run may have one.
    Snapshot { path: PathBuf },
}

impl Default for RunTree {
    fn default() -> Self {
        Self::Fresh {
            repo: None,
            base: None,
            merge_into: None,
        }
    }
}

/// What `start` is asked to start. A request that sets none of the fields after `started_by` (as
/// [`StartRequest::new`] leaves them) starts a task run exactly as it always did.
#[derive(Debug, Clone, PartialEq)]
pub struct StartRequest {
    pub subject: StartSubject,
    /// The workflow to run; `None` is the one the todo names.
    pub workflow: Option<String>,
    /// Input values that win over the definition's sources.
    pub overrides: BTreeMap<String, InputValue>,
    pub started_by: StartedBy,
    /// The id the run is to have, reserved by the caller; `None` makes a new one.
    pub run_id: Option<RunId>,
    /// The run that starts this one.
    pub parent: Option<String>,
    /// The lane running it: its nodes get `BONGOS_LANE`, `BONGOS_LANE_KIND` and
    /// `BONGOS_LANE_DIR_<NAME>`.
    pub lane: Option<LaneRef>,
    /// The lane's named directories, by name: in its nodes' environment and writable by its
    /// programs. Only a lane run may name any.
    pub lane_dirs: BTreeMap<String, PathBuf>,
    pub tree: RunTree,
    /// Edits to the environment of the run's nodes, applied in order.
    pub env: Vec<EnvEdit>,
    pub unattended: Option<UnattendedChoice>,
    /// The commit of the project repository whose definitions the first attempt reads, instead of
    /// the verified commit. The caller reads the layers at it ([`StartContext::definition_commit`]
    /// must be this commit).
    pub definition_commit: Option<CommitId>,
    /// When the workflow already runs its `instances` in the project: queue the run (Queued until
    /// a run of that workflow ends) instead of refusing. Only a hand start may queue; a lane's or
    /// a landing check's start at the limit is always refused.
    pub queue: bool,
}

impl StartRequest {
    /// Kyle starts the todo's own workflow on the current todo of `task_id`.
    pub fn new(task_id: impl Into<String>) -> Self {
        Self::on(StartSubject::Task {
            id: task_id.into(),
            todo_index: None,
        })
    }

    /// A start on the project `project_id`, with no task.
    pub fn project(project_id: impl Into<String>) -> Self {
        Self::on(StartSubject::Project {
            id: project_id.into(),
        })
    }

    fn on(subject: StartSubject) -> Self {
        Self {
            subject,
            workflow: None,
            overrides: BTreeMap::new(),
            started_by: StartedBy::Person,
            run_id: None,
            parent: None,
            lane: None,
            lane_dirs: BTreeMap::new(),
            tree: RunTree::default(),
            env: Vec::new(),
            unattended: None,
            definition_commit: None,
            queue: false,
        }
    }

    /// The task the run is on; `None` for a project subject.
    pub fn task_id(&self) -> Option<&str> {
        self.subject.task_id()
    }
}

/// Why a command was refused, in the order start checks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    UnknownTask,
    UnknownProject,
    /// The request asks for something this version cannot do yet (a lane or snapshot tree, a run
    /// with no task, a dispatcher started as a task run).
    Unsupported,
    /// The request itself is wrong: a reserved run id that is not a UUID v7 or is taken, or a
    /// definition commit the layers were not read from.
    BadRequest,
    /// A run with no task was asked to run a workflow that reads the task.
    NeedsTask,
    Group,
    NoOpenTodo,
    ActiveRun,
    UnknownWorkflow,
    /// The workflow already runs its `instances` in the project.
    InstanceLimit,
    Invalid,
    MissingInput,
    AgentUnavailable,
    InterpreterMissing,
    McpServerMissing,
    LowDiskSpace,
    NoTaskWorktree,
    /// No such run, or the command does not apply in the run's state.
    UnknownRun,
    WrongState,
    /// More than one step failed and the command named none.
    SeveralFailed,
    /// A retry step with the definition read again would lose a completed result.
    FreshDefinition,
    NoGate,
    /// A built-in's start gate was asked to change (an edit, a revision) or to be approved by
    /// a peer that cannot speak for a person.
    StartGate,
    InvalidEdit,
    NoSession,
    /// A restore holds new work in the project back.
    Held,
    Io,
}

/// What a refusal offers to do instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    Resume(RunId),
    Cancel(RunId),
    /// Start again with [`StartRequest::queue`] set.
    Queue,
}

/// A refused command: what is wrong and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub message: String,
    /// What fixes it; shown after the message.
    pub fix: String,
    pub offers: Vec<Offer>,
}

impl Refusal {
    pub fn new(kind: RefusalKind, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            fix: fix.into(),
            offers: Vec::new(),
        }
    }

    fn io(error: impl fmt::Display) -> Self {
        Self::new(RefusalKind::Io, error.to_string(), "")
    }

    fn runtime(error: RuntimeError) -> Self {
        match error {
            RuntimeError::UnknownRun(id) => unknown_run(&id),
            RuntimeError::Over(_) => Self::new(
                RefusalKind::WrongState,
                "The run is over.",
                "Restart it (workflow: restart), or start a new one.",
            ),
            RuntimeError::NoGate(key) => Self::new(
                RefusalKind::NoGate,
                format!("No gate is open at {key}."),
                "Act on the gate the run is waiting at.",
            ),
            RuntimeError::StartGate(message) => Self::new(RefusalKind::StartGate, message, ""),
            other => Self::io(other),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        if !self.fix.is_empty() {
            write!(formatter, " {}", self.fix)?;
        }
        Ok(())
    }
}

impl std::error::Error for Refusal {}

/// Why a peer that cannot attest a person may not approve a built-in's start gate.
pub const PERSON_ONLY_REFUSAL: &str = "approving a Slack post needs a person at a bongos window";

/// Refuses closing the gate of `key` in `state` for a peer that cannot attest a person
/// (`attested` false) when it is a built-in's start gate. Any other gate is left to the verb.
pub fn refuse_unattested(state: &RunState, key: &NodeKey, attested: bool) -> Result<(), Refusal> {
    if !attested && state.person_only_gates.contains(key) {
        return Err(Refusal::new(
            RefusalKind::StartGate,
            PERSON_ONLY_REFUSAL,
            "",
        ));
    }
    Ok(())
}

fn start_gate_as_is() -> Refusal {
    Refusal::new(RefusalKind::StartGate, crate::runtime::START_GATE_AS_IS, "")
}

fn unknown_run(id: &str) -> Refusal {
    Refusal::new(
        RefusalKind::UnknownRun,
        format!("No run {id} is loaded."),
        "Pick a task with an active run.",
    )
}

/// The status a run shows, derived from its replayed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    /// Paused while keys still run; Paused once they finish.
    Pausing,
    Paused,
    Halted,
    /// At an open gate, interrupted by a quit, or held on a check that gave no verdict.
    WaitingOnYou,
    Failed,
    Succeeded,
    Cancelled,
}

impl RunStatus {
    pub fn of(state: &RunState) -> Self {
        if state.cancelled {
            return Self::Cancelled;
        }
        match state.ended {
            Some((EndState::Succeeded, _)) => return Self::Succeeded,
            Some((EndState::Failed, _)) => return Self::Failed,
            None => {}
        }
        if state.halted {
            Self::Halted
        } else if !state.interrupted.is_empty()
            || !state.open_gates.is_empty()
            || !state.held.is_empty()
        {
            Self::WaitingOnYou
        } else if state.paused {
            if state.running.is_empty() {
                Self::Paused
            } else {
                Self::Pausing
            }
        } else if state.running.is_empty() && state.finished.is_empty() {
            Self::Queued
        } else {
            Self::Running
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Halted => "halted",
            Self::WaitingOnYou => "waiting on you",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the run holds its task: every status but succeeded and cancelled. A failed
    /// run stays active until a retry or a cancel closes it out.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Succeeded | Self::Cancelled)
    }

    /// Whether the run moves on by itself: queued, running, or pausing while its keys finish.
    /// Halted, failed, paused and waiting-on-you runs wait for a person.
    pub fn progresses_alone(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Pausing)
    }
}

/// What the lifecycle tells the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowEvent {
    Changed {
        run: RunId,
        task_id: String,
        state: RunStatus,
    },
}

/// The checks of a start that read the machine: agent CLIs, the interpreter and MCP servers.
/// The app answers from 04's health and the program runner's lookup; tests script them.
pub trait StartChecks {
    /// Whether the agent CLI can run; the fix text when it cannot.
    fn agent(&self, kind: AgentKind) -> Result<(), String>;
    /// Whether a program interpreter is there; the fix text when it is not.
    fn interpreter(&self) -> Result<(), String>;
    /// The MCP servers installed for `kind`; `None` when not discovered yet, which skips the
    /// check.
    fn mcp_servers(&self, kind: AgentKind) -> Option<BTreeSet<String>>;
    /// The settings validation reads (ceilings, fan-out width, network, shipped programs).
    fn validation(&self) -> bongos_workflow_model::validate::ValidationContext {
        bongos_workflow_model::validate::ValidationContext::default()
    }
    /// `workflows.mcp_exclude`.
    fn mcp_exclude(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Makes run worktrees. [`GitWorktrees`] in the app; tests may use their own.
pub trait RunWorktrees: Send {
    fn create(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError>;

    /// The branch checked out in the repository `repo`'s main checkout, which a run on the
    /// repository with no base named starts from; `None` when it cannot tell.
    fn base_branch(&mut self, _repo: &Path) -> Option<String> {
        None
    }

    /// The run worktree of a task run in a lane's tree at `path`, which the lane already put on
    /// the run branch ([`worktrees::lane_run`]); `common_dir` is the repository's trusted git
    /// common directory. By default no run is made in a lane's tree.
    fn lane(
        &mut self,
        _task: &TaskCheckout,
        _run_id: &str,
        _attempt: u32,
        path: &Path,
        _common_dir: Option<&Path>,
        _settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        Err(WorktreeError::Io {
            path: path.to_path_buf(),
            message: "no run is made in a lane's tree here".to_string(),
        })
    }
}

/// The free bytes on the disk holding a path ([`worktrees::disk_free_bytes`] in the app).
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// Run worktrees made with git ([`worktrees::create_run`]): a new branch from the task branch,
/// after the free-space check.
pub struct GitWorktrees {
    pub git: Git,
    pub free_space: FreeSpace,
}

impl GitWorktrees {
    /// Git from `PATH` and the real free-space check.
    pub fn system() -> Self {
        Self {
            git: Git::new("git"),
            free_space: Arc::new(worktrees::disk_free_bytes),
        }
    }
}

impl RunWorktrees for GitWorktrees {
    fn create(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        let free_space = self.free_space.clone();
        futures::executor::block_on(worktrees::create_run(
            &self.git,
            task,
            run_id,
            attempt,
            settings,
            &move |path: &Path| free_space(path),
        ))
    }

    fn base_branch(&mut self, repo: &Path) -> Option<String> {
        futures::executor::block_on(worktrees::checked_out_branch(&self.git, repo))
    }

    fn lane(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        path: &Path,
        common_dir: Option<&Path>,
        settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        let free_space = self.free_space.clone();
        futures::executor::block_on(worktrees::lane_run(
            &self.git,
            task,
            run_id,
            attempt,
            path,
            common_dir,
            settings,
            &move |path: &Path| free_space(path),
        ))
    }
}

/// What the app knows at a start, read before the lifecycle is locked.
pub struct StartContext<'a> {
    pub tree: &'a WorkTree,
    /// The run a live-modes source reports on the task (`can_start_run`), if any.
    pub active_run: Option<ActiveRun>,
    pub checks: &'a dyn StartChecks,
    /// The commit of the project repository the project layer of the dirs was read from, which
    /// the first attempt records; `None` when the layer is not a repository's.
    pub definition_commit: Option<String>,
    /// The definitions tree the agents of a run with no task are pointed at when the run is on no
    /// snapshot: the project repository's checkout of them. `None` points them at the project's
    /// own folder.
    pub definitions: Option<PathBuf>,
}

/// What the success end of a run with no task does ([`Lifecycle::project_end`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEnd {
    /// The merge of its run branch, when its start named a branch to merge into.
    pub merge: Option<ProjectMergeRequest>,
    /// The merge is recorded already.
    pub merged: bool,
    /// The `document` outcomes to copy into the project's files folder once merged.
    pub documents: Option<crate::outcomes::ProjectDocuments>,
}

/// How the success end of a run with no task settles ([`project_settlement`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSettled {
    pub settlement: Settlement,
    /// The new head of the branch merged into, to record before settling.
    pub merged_head: Option<String>,
}

/// The success end of a run with no task: with a branch to merge into, the run branch is merged
/// into it in the run tree ([`merge_project_run`]); when another worktree (a person's checkout
/// included) has that branch out, the run waits on a person at the merge gate with the run
/// branch and every checkout as they were, and once the person approves the merge is tried
/// again (nothing is merged when the branch already holds the run's commits). A conflict or any
/// other refusal fails the run, the run branch intact. With no branch to merge into, or a merge
/// already recorded, nothing is merged. Once merged (or with nothing to merge) the run's
/// `document` outcomes are copied into the project's files folder
/// ([`crate::outcomes::deliver_project_documents`]); a promised document that could not be
/// delivered fails the run, else it succeeds.
pub async fn project_settlement(git: &Git, end: &ProjectEnd) -> ProjectSettled {
    let succeed = |merged_head| {
        let settlement = match end
            .documents
            .as_ref()
            .map(crate::outcomes::deliver_project_documents)
        {
            Some(Err(failure)) => Settlement::Fail(failure),
            _ => Settlement::Succeed,
        };
        ProjectSettled {
            settlement,
            merged_head,
        }
    };
    let Some(request) = end.merge.as_ref().filter(|_| !end.merged) else {
        return succeed(None);
    };
    let fail = |reason, detail: String| ProjectSettled {
        settlement: Settlement::Fail(bongos_workflow_model::Failure::new(reason, None, detail)),
        merged_head: None,
    };
    match merge_project_run(git, request).await {
        Ok(ProjectMerged::Merged { head }) => succeed(Some(head)),
        Ok(ProjectMerged::AlreadyIn) => succeed(None),
        Err(refusal @ ProjectMergeRefusal::CheckedOut { .. }) => ProjectSettled {
            settlement: Settlement::Wait {
                key: crate::task_link::gate_key(crate::task_link::MERGE_GATE),
                question: refusal.to_string(),
            },
            merged_head: None,
        },
        Err(refusal @ ProjectMergeRefusal::Conflict(_)) => fail(
            bongos_workflow_model::FailureReason::MergeConflict,
            refusal.to_string(),
        ),
        Err(refusal) => fail(
            bongos_workflow_model::FailureReason::WorktreeUnavailable,
            refusal.to_string(),
        ),
    }
}

/// Where a halted run can be edited: its worktree and its fan-out branch worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaltWorktrees {
    pub run: PathBuf,
    pub branches: Vec<PathBuf>,
}

/// The marker file the `workflow: clear finished` action will leave in a run directory it
/// clears; a run with it is never loaded.
pub const CLEARED_FILE: &str = "cleared";

/// What [`Lifecycle::clear_finished`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClearOutcome {
    /// The runs that left the lifecycle: their marker is written.
    pub cleared: Vec<RunId>,
    /// The runs that stay listed because their marker could not be written, with the error.
    pub failed: Vec<(RunId, String)>,
}

/// A run directory that holds the `cleared` marker of a run that ended succeeded or was
/// cancelled: the worktrees it left are offered for removal by `workflow: clear finished`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClearedRun {
    pub id: RunId,
    /// `<task dir>/runs/<id>`.
    pub dir: PathBuf,
    /// The worktree of the current attempt, as the record says.
    pub worktree: RunWorktree,
    /// The number of the current attempt.
    pub attempt: u32,
    /// The task the run is on, from its own task file: its worktree names the repository.
    pub checkout: TaskCheckout,
    pub status: RunStatus,
    /// When the attempt ended or was cancelled: the modification time of its event log.
    pub finished: std::time::SystemTime,
}

/// The cleared runs under every task directory of `tree`, with the worktree of their current
/// attempt. A directory whose record or log cannot be read, and a run that is not succeeded or
/// cancelled, is skipped (logged): only a finished run is ever offered for removal. Reads only.
pub fn find_cleared(tree: &WorkTree) -> Vec<ClearedRun> {
    let mut found = Vec::new();
    for task in tree.tasks() {
        let runs = bongos_work_model::layout::runs_dir(&task.dir);
        let Ok(listing) = fs::read_dir(&runs) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = listing
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.join(CLEARED_FILE).exists())
            .collect();
        dirs.sort();
        let checkout = match &task.file {
            Some(file) => TaskCheckout::from_task(task.slug.clone(), file),
            None => TaskCheckout {
                slug: task.slug.clone(),
                worktree: None,
                branch: None,
            },
        };
        for dir in dirs {
            match read_cleared(&dir, &checkout) {
                Ok(Some(run)) => found.push(run),
                Ok(None) => {}
                Err(reason) => log::warn!(
                    "workflows: cannot offer the worktrees of {}: {reason}",
                    dir.display()
                ),
            }
        }
    }
    found
}

fn read_cleared(dir: &Path, checkout: &TaskCheckout) -> Result<Option<ClearedRun>, String> {
    let text =
        fs::read_to_string(dir.join(RUN_FILE)).map_err(|error| format!("{RUN_FILE}: {error}"))?;
    let record = RunToml::parse(&text).map_err(|error| error.to_string())?;
    if record.tree.is_some() {
        // A snapshot or a project's repository tree is not a task run worktree: never offered
        // for removal here.
        return Ok(None);
    }
    let attempt = record
        .current_attempt()
        .ok_or_else(|| format!("{RUN_FILE} has no attempt"))?;
    let events = attempt_dir(dir, attempt.n).join(crate::runtime::EVENTS_FILE);
    let bytes = fs::read(&events).map_err(|error| format!("{}: {error}", events.display()))?;
    let parsed =
        bongos_workflow_model::record::parse_log(&bytes).map_err(|error| error.to_string())?;
    let status = RunStatus::of(&bongos_workflow_model::record::replay(&parsed.events));
    if !matches!(status, RunStatus::Succeeded | RunStatus::Cancelled) {
        return Ok(None);
    }
    let finished = fs::metadata(&events)
        .and_then(|metadata| metadata.modified())
        // Unknown: never old enough to be offered.
        .unwrap_or_else(|_| std::time::SystemTime::now());
    Ok(Some(ClearedRun {
        id: record.id.clone(),
        dir: dir.to_path_buf(),
        worktree: attempt_worktree(attempt, checkout)?,
        attempt: attempt.n,
        checkout: checkout.clone(),
        status,
        finished,
    }))
}

/// The refusal of a start whose run worktree was not made.
fn worktree_refusal(error: WorktreeError) -> Refusal {
    match error {
        WorktreeError::NotStarted => Refusal::new(
            RefusalKind::NoTaskWorktree,
            "This task has no worktree.",
            "Start the task first (task: start).",
        ),
        WorktreeError::LowDiskSpace { .. } => Refusal::new(
            RefusalKind::LowDiskSpace,
            format!("Not enough free space: {error}."),
            "Free some space, or lower workflows.min_free_disk_mb.",
        ),
        other => Refusal::io(other),
    }
}

/// The run worktree of `attempt`. A record that has no git directory (one written before the field)
/// gets the one under the common directory of the task's repository that names the tree, trusted
/// only where git puts it ([`linked_git_dir_from_record`]); a `.git` that names anything else
/// refuses. The common directory comes from the task's own worktree.
pub(crate) fn attempt_worktree(
    attempt: &Attempt,
    checkout: &TaskCheckout,
) -> Result<RunWorktree, String> {
    let path = PathBuf::from(&attempt.worktree);
    let git_dir = match &attempt.git_dir {
        Some(git_dir) if !git_dir.is_empty() => PathBuf::from(git_dir),
        // The tree is gone: no git call runs in it, and its branch goes alone (see `run_worktrees`).
        _ if !path.exists() => PathBuf::new(),
        _ => {
            let task_worktree = checkout.worktree.as_deref().filter(|tree| tree.is_dir());
            let task_worktree = task_worktree.ok_or_else(|| {
                "the task worktree is gone, so the git directory of the run worktree is unknown"
                    .to_string()
            })?;
            let common = futures::executor::block_on(Git::new("git").run(
                task_worktree,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ))
            .map_err(|error| error.to_string())?;
            linked_git_dir_from_record(&path, Path::new(&common))
                .map_err(|error| format!("{error:#}"))?
        }
    };
    Ok(RunWorktree {
        run_dir: path
            .parent()
            .map_or_else(|| path.clone(), Path::to_path_buf),
        path,
        branch: attempt.branch.clone(),
        git_dir,
    })
}

/// One output field of a gate's held result, for `workflow: approve` with an edit.
#[derive(Debug, Clone, PartialEq)]
pub struct GateField {
    pub name: String,
    pub field_type: bongos_workflow_model::definition::FieldType,
    /// The held value.
    pub value: Value,
}

/// What [`Lifecycle::load_runs`] did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LoadReport {
    /// The runs loaded, in load order.
    pub loaded: Vec<RunId>,
    /// Each loaded run's orphan processes and what became of them.
    pub orphans: Vec<(RunId, Vec<(NodeKey, crate::process::OrphanOutcome)>)>,
    /// Run directories (or task `runs/` directories) skipped, each with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

fn skip(report: &mut LoadReport, path: &Path, reason: String) {
    log::warn!("workflows: skipped {}: {reason}", path.display());
    report.skipped.push((path.to_path_buf(), reason));
}

/// A run the lifecycle knows.
struct RunEntry {
    id: RunId,
    task_id: String,
    /// The slug of the project holding the task; `None` for a one-off.
    project: Option<String>,
    task: TaskCheckout,
    /// `<task dir>/runs/<id>`.
    dir: PathBuf,
    record: RunToml,
    worktree: RunWorktree,
    /// The definition revision in use within the current attempt.
    rev: u32,
    /// What the run was started with besides its task.
    setup: RunSetup,
}

impl RunEntry {
    fn attempt(&self) -> u32 {
        self.record.current_attempt().map_or(1, |attempt| attempt.n)
    }

    fn attempt_dir(&self) -> PathBuf {
        attempt_dir(&self.dir, self.attempt())
    }

    /// The context a runner gets for this run's current attempt.
    fn binding(&self) -> RunBinding {
        let task_dir = self
            .dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&self.dir)
            .to_path_buf();
        RunBinding {
            workflow: self.record.workflow.clone(),
            task_id: self.task_id.clone(),
            task_dir,
            run_dir: self.dir.clone(),
            attempt_dir: self.attempt_dir(),
            worktree: self.worktree.clone(),
            checkout: self.task.clone(),
            inputs: self
                .record
                .inputs
                .iter()
                .map(|input| bongos_workflow_model::inputs::Input {
                    name: input.name.clone(),
                    value: input.value.clone(),
                    source: input.source.clone(),
                    missing: false,
                })
                .collect(),
            setup: self.setup.clone(),
        }
    }
}

/// The setup of a run as its record and the work tree have it: the lane, its directories and the
/// environment edits from the record, whether the tree is a snapshot, and for a project subject
/// the project of `tree` with its files folder (`project_dir`) and the definitions to point its
/// agents at.
fn setup_of(record: &RunToml, tree: &WorkTree, project_dir: &Path) -> RunSetup {
    let snapshot = match &record.tree {
        Some(RunTreeRecord::Snapshot { path }) => Some(path.clone()),
        _ => None,
    };
    let project = match &record.subject {
        RunSubject::Project { id } => tree
            .projects
            .iter()
            .find(|project| project.task.id() == Some(id.as_str()))
            .map(|project| ProjectSource {
                project: project.clone(),
                files_dir: project_dir.to_path_buf(),
                definitions: snapshot
                    .clone()
                    .or_else(|| record.definitions.clone())
                    .unwrap_or_else(|| project_dir.to_path_buf()),
            }),
        RunSubject::Task { .. } => None,
    };
    RunSetup {
        lane: record.lane.clone(),
        lane_dirs: record.lane_dirs.clone(),
        env: record.env.clone(),
        snapshot: snapshot.is_some(),
        work_dir: matches!(record.tree, Some(RunTreeRecord::Work)),
        project,
    }
}

/// `<project dir>/work/<run id>`: where a run with no task keeps what it writes besides its tree
/// (the scratch directories of its programs), beside its records and never inside them. A run
/// with no repository and no snapshot works in it.
pub fn project_work_dir(project_dir: &Path, run_id: &str) -> PathBuf {
    project_dir.join(PROJECT_WORK_DIR).join(run_id)
}

/// `run_id` as one plain path name: not empty, no separator, not `.` or `..`.
fn plain_name(run_id: &str) -> Option<&str> {
    let mut components = Path::new(run_id).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(name)), None) if name.to_str() == Some(run_id) => {
            Some(run_id)
        }
        _ => None,
    }
}

/// Makes the work directory of a new run with no repository, `<project dir>/work/<run id>`,
/// with its parent. Refused when `work` is not a real directory of the project folder (a
/// symbolic link could lead the run's writes anywhere, its records included) or the run's own
/// directory is already there.
fn create_project_work_dir(project_dir: &Path, run_id: &str) -> std::io::Result<PathBuf> {
    let invalid = |message: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
    let run_id = plain_name(run_id).ok_or_else(|| invalid(format!("bad run id '{run_id}'")))?;
    let parent = project_dir.join(PROJECT_WORK_DIR);
    match fs::symlink_metadata(&parent) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(invalid(format!(
                "{} is not a directory of the project folder",
                parent.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(&parent)?,
        Err(error) => return Err(error),
    }
    let dir = parent.join(run_id);
    fs::create_dir(&dir)?;
    Ok(dir)
}

/// Removes the work directory of the run `run_id` with no repository, `<project dir>/work/<run
/// id>`, and only that: a run id that is not one plain name, a `work` that is not a real
/// directory, and a run directory that is a symbolic link or no directory are refused, and
/// nothing is removed. A directory already gone is `Ok(false)`. Links inside it are removed, never
/// followed.
pub fn remove_project_work_dir(project_dir: &Path, run_id: &str) -> std::io::Result<bool> {
    let invalid = |message: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
    let run_id = plain_name(run_id).ok_or_else(|| invalid(format!("bad run id '{run_id}'")))?;
    let parent = project_dir.join(PROJECT_WORK_DIR);
    let dir = parent.join(run_id);
    for path in [&parent, &dir] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(invalid(format!(
                    "{} is not a real directory",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    fs::remove_dir_all(&dir)?;
    Ok(true)
}

/// The first of `writable` that `snapshot` contains or lies inside, symlinks resolved, so a
/// read-only snapshot never shares files with a directory its run may write.
fn snapshot_overlap<'a>(
    snapshot: &Path,
    writable: impl IntoIterator<Item = &'a Path>,
) -> Option<&'a Path> {
    let snapshot = resolved(snapshot);
    writable.into_iter().find(|dir| {
        let dir = resolved(dir);
        dir.starts_with(&snapshot) || snapshot.starts_with(&dir)
    })
}

/// `path` with symlinks resolved as far as it exists; the rest is appended as written.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return rest
                .into_iter()
                .rev()
                .fold(canonical, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The directory of a project's files folder holding the per-run work directories.
pub const PROJECT_WORK_DIR: &str = "work";

/// `<run dir>/attempts/<n>`.
pub fn attempt_dir(run_dir: &Path, attempt: u32) -> PathBuf {
    run_dir.join("attempts").join(attempt.to_string())
}

/// The active run of each task of `tree` (by task id), read from the run records in each task's
/// folder (`runs/<id>`), for a process with no run engine loaded: a read mode, or a window that
/// does not hold the writer. Active is what holds a task ([`RunStatus::is_active`]), as the
/// current attempt's log says. A record that cannot be read is skipped; of two active runs on one
/// task the newest (run ids are UUID v7) is named.
pub fn active_runs_on_disk(tree: &WorkTree) -> BTreeMap<String, RunId> {
    let mut active: BTreeMap<String, RunId> = BTreeMap::new();
    for task in tree.tasks() {
        let Some(task_id) = task.id() else {
            continue;
        };
        let Ok(entries) = fs::read_dir(task.dir.join(RUNS_DIR)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some((run, status)) = recorded_status(&entry.path()) else {
                continue;
            };
            if !status.is_active() {
                continue;
            }
            let newer = active.get(task_id).is_none_or(|known| *known < run);
            if newer {
                active.insert(task_id.to_string(), run);
            }
        }
    }
    active
}

/// The id and status of the run whose directory is `dir`, from its `run.toml` and the log of its
/// current attempt; `None` when either cannot be read.
fn recorded_status(dir: &Path) -> Option<(RunId, RunStatus)> {
    let text = fs::read_to_string(dir.join(RUN_FILE)).ok()?;
    let record = RunToml::parse(&text).ok()?;
    let attempt = record.current_attempt()?;
    let events = attempt_dir(dir, attempt.n).join(crate::runtime::EVENTS_FILE);
    let parsed = bongos_workflow_model::record::parse_log(fs::read(events).ok()?).ok()?;
    let state = bongos_workflow_model::record::replay(&parsed.events);
    Some((record.id, RunStatus::of(&state)))
}

/// `<attempt dir>/definition/<rev>`.
pub fn definition_copy_dir(attempt_dir: &Path, rev: u32) -> PathBuf {
    attempt_dir.join("definition").join(rev.to_string())
}

/// The live runs and their records, over a [`Runtime`].
pub struct Lifecycle<R: Runner> {
    runtime: Runtime<R>,
    worktrees: Box<dyn RunWorktrees>,
    settings: WorkflowsSettings,
    runs: Vec<RunEntry>,
    /// What the last event for a run said: its status and its open gates, so a second gate in a
    /// run that already waits is an update too.
    statuses: BTreeMap<RunId, (RunStatus, BTreeSet<NodeKey>)>,
    events: Vec<WorkflowEvent>,
    /// The files of the projects whose writer moves to another process now: no run starts under
    /// them ([`Self::refuse_starts_under`]).
    moving: Vec<PathBuf>,
    /// The runs of the projects another process writes, read from their records to be shown
    /// ([`Self::observe_runs`]), in start order. Nothing drives them here and no command reaches
    /// them: every command looks among `runs` only.
    observed: Vec<ObservedRun>,
}

/// A run another process drives, as its records on disk say.
struct ObservedRun {
    entry: RunEntry,
    definition: Definition,
    /// The replayed log of the current attempt.
    state: RunState,
    /// What `run.toml`, the current attempt's log and the `cleared` marker looked like when they
    /// were read: the run is read again once one differs.
    stamps: [FileStamp; 3],
}

/// The modification time and length of a file, `None` when it cannot be read.
type FileStamp = Option<(std::time::SystemTime, u64)>;

fn file_stamp(path: &Path) -> FileStamp {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// The stamps of the files of run directory `dir` whose current attempt's log is `events`.
fn run_stamps(dir: &Path, events: &Path) -> [FileStamp; 3] {
    [
        file_stamp(&dir.join(RUN_FILE)),
        file_stamp(events),
        file_stamp(&dir.join(CLEARED_FILE)),
    ]
}

impl<R: Runner> Lifecycle<R> {
    pub fn new(runner: R, worktrees: Box<dyn RunWorktrees>, settings: WorkflowsSettings) -> Self {
        Self {
            runtime: Runtime::new(settings.limits, runner),
            worktrees,
            settings,
            runs: Vec::new(),
            statuses: BTreeMap::new(),
            events: Vec::new(),
            moving: Vec::new(),
            observed: Vec::new(),
        }
    }

    /// While `on`, a start or restart of a run under `files` (a project's files) is refused: the
    /// project's writer moves to another process, which loads the project's runs from disk, so
    /// a run this process began now would run in both.
    pub fn refuse_starts_under(&mut self, files: &Path, on: bool) {
        self.moving.retain(|dir| dir != files);
        if on {
            self.moving.push(files.to_path_buf());
        }
    }

    /// Refuses a run under `dir` while its project's writer moves.
    fn refuse_moving(&self, dir: &Path) -> Result<(), Refusal> {
        if self.moving.iter().any(|files| dir.starts_with(files)) {
            return Err(Refusal::new(
                RefusalKind::Held,
                "This project's writer is moving to the engine host, so no run starts in it now.",
                "Try again once the move is over.",
            ));
        }
        Ok(())
    }

    pub fn runtime(&self) -> &Runtime<R> {
        &self.runtime
    }

    pub fn runtime_mut(&mut self) -> &mut Runtime<R> {
        &mut self.runtime
    }

    /// Takes the settings a later start or restart reads (free space, worktree directory).
    pub fn set_settings(&mut self, settings: WorkflowsSettings) {
        self.runtime.set_limits(settings.limits);
        self.settings = settings;
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<WorkflowEvent> {
        std::mem::take(&mut self.events)
    }

    /// One pass of the run loop ([`Runtime::tick`]); whether anything changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = self.runtime.tick();
        // A run that ended may free an instance for a queued run of its workflow.
        if self.release_instances() {
            self.runtime.tick();
            changed = true;
        }
        if changed {
            self.note_all();
        }
        changed
    }

    /// The replayed state of the current attempt of `run`.
    pub fn state(&self, run: &str) -> Option<RunState> {
        self.runtime.state(run)
    }

    pub fn status(&self, run: &str) -> Option<RunStatus> {
        self.state(run).map(|state| RunStatus::of(&state))
    }

    /// The run worktree of the current attempt of `run`.
    pub fn worktree(&self, run: &str) -> Option<&RunWorktree> {
        self.entry(run).ok().map(|entry| &entry.worktree)
    }

    /// The directory of `run`'s record (`<task dir>/runs/<id>`).
    pub fn run_dir(&self, run: &str) -> Option<&Path> {
        self.entry(run).ok().map(|entry| entry.dir.as_path())
    }

    /// The record of `run` as written to `run.toml`.
    pub fn record(&self, run: &str) -> Option<&RunToml> {
        self.entry(run).ok().map(|entry| &entry.record)
    }

    /// The task `run` belongs to.
    pub fn task_of(&self, run: &str) -> Option<&str> {
        self.entry(run).ok().map(|entry| entry.task_id.as_str())
    }

    /// The active run of `task_id`, if any.
    pub fn active_run_id(&self, task_id: &str) -> Option<RunId> {
        self.runs
            .iter()
            .filter(|entry| entry.task_id == task_id)
            .find(|entry| self.status(&entry.id).is_some_and(RunStatus::is_active))
            .map(|entry| entry.id.clone())
    }

    /// Every run, active or finished, in start order.
    pub fn all_runs(&self) -> Vec<RunId> {
        self.runs.iter().map(|entry| entry.id.clone()).collect()
    }

    /// Every active run, in start order.
    pub fn active_runs(&self) -> Vec<RunId> {
        self.runs
            .iter()
            .filter(|entry| self.status(&entry.id).is_some_and(RunStatus::is_active))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Every run that moves on by itself (see [`RunStatus::progresses_alone`]), in start order.
    pub fn progressing_runs(&self) -> Vec<RunId> {
        self.runs
            .iter()
            .filter(|entry| {
                self.status(&entry.id)
                    .is_some_and(RunStatus::progresses_alone)
            })
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// The active run of `task_id` as the `workflows` live-modes source reports it.
    pub fn active_run(&self, task_id: &str) -> Option<ActiveRun> {
        let id = self.active_run_id(task_id)?;
        let state = self.state(&id)?;
        let entry = self.entry(&id).ok()?;
        let past_runs = self
            .runs
            .iter()
            .filter(|other| other.task_id == task_id && other.id != id)
            .count();
        Some(ActiveRun {
            workflow: entry.record.workflow.clone(),
            status: RunStatus::of(&state).label().to_string(),
            step: step_names(&state),
            past_runs,
        })
    }

    /// The open gates of `run`.
    pub fn open_gates(&self, run: &str) -> Vec<NodeKey> {
        self.state(run)
            .map(|state| state.open_gates.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The task checkout `run` was started from (its worktree and branch).
    pub fn task_checkout(&self, run: &str) -> Option<&TaskCheckout> {
        self.entry(run).ok().map(|entry| &entry.task)
    }

    /// The directory of the current attempt of `run`.
    pub fn current_attempt_dir(&self, run: &str) -> Option<PathBuf> {
        self.entry(run).ok().map(RunEntry::attempt_dir)
    }

    /// Every run shown in the views, in start order: the runs this process drives, then those it
    /// reads for another process ([`Self::observe_runs`]).
    pub fn shown_runs(&self) -> Vec<RunId> {
        self.runs
            .iter()
            .chain(self.observed.iter().map(|observed| &observed.entry))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Every active run of [`Self::shown_runs`], in that order.
    pub fn shown_active_runs(&self) -> Vec<RunId> {
        self.shown_runs()
            .into_iter()
            .filter(|run| {
                self.shown_state(run)
                    .is_some_and(|state| RunStatus::of(&state).is_active())
            })
            .collect()
    }

    /// Whether `run` is one another process drives, shown here from its records.
    pub fn is_observed(&self, run: &str) -> bool {
        self.observed_run(run).is_some()
    }

    /// [`Self::state`] of a shown run: for one another process drives, its log as last read.
    pub fn shown_state(&self, run: &str) -> Option<RunState> {
        self.state(run).or_else(|| {
            self.observed_run(run)
                .map(|observed| observed.state.clone())
        })
    }

    /// The definition a shown run runs on.
    pub fn shown_definition(&self, run: &str) -> Option<&Definition> {
        self.runtime
            .definition(run)
            .or_else(|| self.observed_run(run).map(|observed| &observed.definition))
    }

    /// [`Self::record`] of a shown run.
    pub fn shown_record(&self, run: &str) -> Option<&RunToml> {
        self.shown_entry(run).map(|entry| &entry.record)
    }

    /// [`Self::task_of`] of a shown run.
    pub fn shown_task_of(&self, run: &str) -> Option<&str> {
        self.shown_entry(run).map(|entry| entry.task_id.as_str())
    }

    /// [`Self::task_checkout`] of a shown run.
    pub fn shown_task_checkout(&self, run: &str) -> Option<&TaskCheckout> {
        self.shown_entry(run).map(|entry| &entry.task)
    }

    /// [`Self::worktree`] of a shown run.
    pub fn shown_worktree(&self, run: &str) -> Option<&RunWorktree> {
        self.shown_entry(run).map(|entry| &entry.worktree)
    }

    /// [`Self::current_attempt_dir`] of a shown run.
    pub fn shown_attempt_dir(&self, run: &str) -> Option<PathBuf> {
        self.shown_entry(run).map(RunEntry::attempt_dir)
    }

    fn observed_run(&self, run: &str) -> Option<&ObservedRun> {
        self.observed
            .iter()
            .find(|observed| observed.entry.id == run)
    }

    /// The entry of a run this process drives, else of one it shows for another process.
    fn shown_entry(&self, run: &str) -> Option<&RunEntry> {
        match self.runs.iter().find(|entry| entry.id == run) {
            Some(entry) => Some(entry),
            None => self.observed_run(run).map(|observed| &observed.entry),
        }
    }

    /// Reads the runs of every task under `under` (the files folders of the projects whose writer
    /// another process holds) from their records, to be shown and never driven: no log is opened
    /// for writing or repaired, no process is touched, no lock is taken, and no event is raised
    /// for them, so nothing is written for them either. A run whose `run.toml`, current log and
    /// `cleared` marker look as when last read is kept as it was; any other is read again. A
    /// cleared run, a run this process drives and a run that cannot be read are left out. With
    /// `under` empty every observed run is dropped. Whether what is shown changed.
    pub fn observe_runs(&mut self, tree: &WorkTree, under: &[PathBuf]) -> bool {
        // `observed` is in id order, as it is left below.
        let before: Vec<(RunId, RunState, RunToml)> = self
            .observed
            .iter()
            .map(|observed| {
                (
                    observed.entry.id.clone(),
                    observed.state.clone(),
                    observed.entry.record.clone(),
                )
            })
            .collect();
        let mut previous: BTreeMap<PathBuf, ObservedRun> = std::mem::take(&mut self.observed)
            .into_iter()
            .map(|observed| (observed.entry.dir.clone(), observed))
            .collect();
        let mut observed: Vec<ObservedRun> = Vec::new();
        let mut seen: BTreeSet<RunId> = BTreeSet::new();
        for task in tree.tasks() {
            if !under.iter().any(|files| task.dir.starts_with(files)) {
                continue;
            }
            let runs = bongos_work_model::layout::runs_dir(&task.dir);
            let Ok(listing) = fs::read_dir(&runs) else {
                continue;
            };
            let mut dirs: Vec<PathBuf> = listing
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            dirs.sort();
            let project = task.id().and_then(|id| project_of(tree, id));
            for dir in dirs {
                let kept = previous.remove(&dir).filter(|known| {
                    let events = known.entry.attempt_dir().join(crate::runtime::EVENTS_FILE);
                    run_stamps(&dir, &events) == known.stamps
                });
                let run = match kept {
                    Some(known) => known,
                    None => {
                        // Stamped before the read, and the log with the length that was read, so
                        // a write while reading makes the stamps differ and the run is read again.
                        let record_stamp = file_stamp(&dir.join(RUN_FILE));
                        let cleared_stamp = file_stamp(&dir.join(CLEARED_FILE));
                        match self.read_run_state(tree, task, project.clone(), &dir) {
                            Ok(Some((entry, spec, state, read_len))) => {
                                let events = spec.attempt_dir.join(crate::runtime::EVENTS_FILE);
                                let log_stamp =
                                    file_stamp(&events).map(|(time, _)| (time, read_len));
                                ObservedRun {
                                    stamps: [record_stamp, log_stamp, cleared_stamp],
                                    entry,
                                    definition: spec.definition,
                                    state,
                                }
                            }
                            Ok(None) => continue,
                            Err(reason) => {
                                log::debug!("workflows: not showing {}: {reason}", dir.display());
                                continue;
                            }
                        }
                    }
                };
                // A cleared run reads as `Ok(None)` above: its marker changes its stamps.
                if self.runs.iter().any(|entry| entry.id == run.entry.id)
                    || !seen.insert(run.entry.id.clone())
                {
                    continue;
                }
                observed.push(run);
            }
        }
        // Run ids are UUID v7, so their order is the order the runs were created in.
        observed.sort_by(|a, b| a.entry.id.cmp(&b.entry.id));
        let changed = before.len() != observed.len()
            || before
                .iter()
                .zip(&observed)
                .any(|((id, state, record), now)| {
                    *id != now.entry.id || *state != now.state || *record != now.entry.record
                });
        self.observed = observed;
        changed
    }

    /// What the runner is given for `run`'s current attempt.
    pub fn binding(&self, run: &str) -> Option<RunBinding> {
        self.entry(run).ok().map(RunEntry::binding)
    }

    /// What the success end of `run` does when it is a run with no task; `None` for a task run.
    pub fn project_end(&self, run: &str) -> Option<ProjectEnd> {
        let entry = self.entry(run).ok()?;
        if !matches!(entry.record.subject, RunSubject::Project { .. }) {
            return None;
        }
        let merge_into = match &entry.record.tree {
            Some(RunTreeRecord::Repository { merge_into, .. }) => merge_into.clone(),
            _ => None,
        };
        let state = self.runtime.state(run)?;
        let documents = self.runtime.definition(run).and_then(|definition| {
            use crate::outcomes::{self, ProjectDocument, ProjectDocuments, RunOutcome};
            let files_dir = entry.dir.parent()?.parent()?.to_path_buf();
            let documents = outcomes::collect(definition, &state)
                .into_iter()
                .filter_map(|reported| match reported.outcome {
                    RunOutcome::Document { path } => Some(ProjectDocument {
                        root: outcomes::source_root(&entry.worktree, &reported.key, &path),
                        key: reported.key,
                        path,
                    }),
                    _ => None,
                })
                .collect();
            let limits = self.runtime.run_limits(run);
            Some(ProjectDocuments {
                files_dir,
                run_id: entry.id.clone(),
                documents,
                promised: outcomes::promised(definition, &state, &limits)
                    .contains(&bongos_workflow_model::definition::Outcome::Document),
                marker: entry.attempt_dir().join(crate::engine::DELIVERED_FILE),
            })
        });
        Some(ProjectEnd {
            documents,
            merge: merge_into.map(|merge_into| ProjectMergeRequest {
                run_tree: entry.worktree.path.clone(),
                run_git_dir: entry.worktree.git_dir.clone(),
                run_branch: entry.worktree.branch.clone(),
                merge_into,
                run_id: entry.id.clone(),
                workflow: entry.record.workflow.clone(),
            }),
            merged: state
                .merged
                .iter()
                .any(|merged| merged.join == crate::task_link::MERGE_GATE),
        })
    }

    /// The runs held at a succeeded end for the engine since the last call
    /// ([`Runtime::take_settling`]).
    pub fn take_settling(&mut self) -> Vec<RunId> {
        self.runtime.take_settling()
    }

    /// Ends or holds a run taken with [`Lifecycle::take_settling`].
    pub fn settle(&mut self, run: &str, settlement: Settlement) -> Result<(), Refusal> {
        self.entry(run)?;
        self.runtime
            .settle(run, settlement)
            .map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// Records the engine's final merge of `run` ([`Runtime::record_merge`]).
    pub fn record_merge(&mut self, run: &str, join: &str, sha: String) -> Result<(), Refusal> {
        self.entry(run)?;
        self.runtime
            .record_merge(run, join, sha)
            .map_err(Refusal::runtime)
    }

    /// Settles a run with no task as [`project_settlement`] decided: its merge is recorded first,
    /// so a later settlement never merges again; a merge that cannot be recorded fails the run.
    pub fn settle_project(&mut self, run: &str, settled: ProjectSettled) -> Result<(), Refusal> {
        let mut settlement = settled.settlement;
        if let Some(head) = settled.merged_head
            && let Err(refusal) = self.record_merge(run, crate::task_link::MERGE_GATE, head)
        {
            settlement = Settlement::Fail(bongos_workflow_model::Failure::new(
                bongos_workflow_model::FailureReason::WorktreeUnavailable,
                None,
                refusal.to_string(),
            ));
        }
        self.settle(run, settlement)
    }

    /// Starts a run on the task and todo of `request`. A request whose own fields cannot be
    /// honored (a lane tree, a snapshot or repository tree for a task, lane directories with no
    /// lane, a taken or malformed run id, a definition commit the
    /// layers were not read from) is refused first, and a project subject goes to
    /// [`Self::start_project`]. Then refused, in this order: a group, no open
    /// todo, an active run, an unknown workflow, a definition with errors, a missing required
    /// input, an agent CLI that cannot run, a missing interpreter, an allowed MCP server not
    /// installed, the workflow at its `instances` in the project (a hand start that asks to queue
    /// is let through and waits Queued until a run of the workflow ends), too little free space,
    /// no task worktree. Nothing is written before every check
    /// passed but the last two, which the worktree creation makes.
    pub fn start(
        &mut self,
        request: StartRequest,
        dirs: &LayerDirs,
        context: &StartContext<'_>,
    ) -> Result<RunId, Refusal> {
        self.check_request(&request, context)?;
        let (task_id, todo_index) = match &request.subject {
            StartSubject::Task { id, todo_index } => (id.clone(), *todo_index),
            StartSubject::Project { id } => {
                let id = id.clone();
                return self.start_project(request, &id, dirs, context);
            }
        };
        let tree = context.tree;
        let task = tree.task_by_id(&task_id).ok_or_else(|| {
            Refusal::new(
                RefusalKind::UnknownTask,
                format!("No task {task_id} is loaded."),
                "Pick a task in the Work view.",
            )
        })?;
        self.refuse_moving(&task.dir)?;
        if derive::shape(task, tree) == Shape::Group {
            return Err(Refusal::new(
                RefusalKind::Group,
                "Groups have no todos.",
                "Pick a leaf, or use `task: run`.",
            ));
        }
        let (todo_index, todo) = open_todo(task, todo_index).ok_or_else(|| {
            Refusal::new(
                RefusalKind::NoOpenTodo,
                "This task has no open todo for a workflow to complete.",
                "Add one (todo: add), or reopen one.",
            )
        })?;
        let reported = context.active_run.clone();
        if let Some(refusal) = self.active_refusal(&task_id, reported) {
            return Err(refusal);
        }
        let name = request
            .workflow
            .clone()
            .or_else(|| todo.workflow.clone())
            .ok_or_else(|| {
                Refusal::new(
                    RefusalKind::UnknownWorkflow,
                    format!("The todo '{}' names no workflow.", todo.name),
                    "Set its `workflow`, or name one when starting.",
                )
            })?;
        let (resolved, definition) = load_definition(
            dirs,
            &name,
            context.checks.validation(),
            Some(context.checks),
        )?;
        refuse_dispatcher(&name, &definition)?;
        let inputs = start_inputs(
            &name,
            &definition,
            Some(task),
            tree,
            Some(todo_index),
            &request.overrides,
        )?;
        check_machine(&definition, context.checks)?;
        let project = project_of(tree, &task_id);
        let held_back = self.check_instances(&name, &definition, project.as_deref(), &request)?;

        let file = task.file.as_ref().ok_or_else(|| {
            Refusal::new(
                RefusalKind::NoTaskWorktree,
                "This task has no worktree.",
                "Start the task first (task: start).",
            )
        })?;
        let checkout = TaskCheckout::from_task(task.slug.clone(), file);
        let id = request.run_id.clone().unwrap_or_else(new_id);
        let (worktree, tree_record) = match &request.tree {
            RunTree::Lane {
                path, common_dir, ..
            } => {
                let worktree = self.lane_worktree(&checkout, &id, path, common_dir.as_deref())?;
                let record = RunTreeRecord::Lane {
                    path: worktree.path.clone(),
                    run_dir: worktree.run_dir.clone(),
                };
                (worktree, Some(record))
            }
            _ => (self.create_worktree(&checkout, &id, 1)?, None),
        };
        let dir = task.dir.join(RUNS_DIR).join(&id);
        let created = now().to_string();
        let record = RunToml {
            schema: RUN_SCHEMA,
            id: id.clone(),
            workflow: name,
            layer: resolved.layer.label().to_string(),
            subject: RunSubject::Task {
                id: task_id.clone(),
            },
            task_id: task_id.clone(),
            created: created.clone(),
            started_by: request.started_by,
            parent: request.parent.clone(),
            lane: request.lane.clone(),
            unattended: request.unattended,
            tree: tree_record,
            definitions: None,
            env: request.env.clone(),
            lane_dirs: request.lane_dirs,
            todo: Some(TodoRef {
                index: todo_index,
                name: todo.name.clone(),
            }),
            inputs: inputs
                .into_iter()
                .map(|input| RecordedInput {
                    name: input.name,
                    source: input.source,
                    value: input.value,
                })
                .collect(),
            attempts: Vec::new(),
        };
        let setup = setup_of(&record, tree, &task.dir);
        let mut entry = RunEntry {
            id: id.clone(),
            task_id,
            project,
            task: checkout,
            dir,
            record,
            worktree,
            rev: 0,
            setup,
        };
        let commit = context
            .definition_commit
            .clone()
            .filter(|_| resolved.layer == Layer::Project);
        self.begin_attempt(&mut entry, 1, &resolved.dir, definition, created, commit)?;
        if held_back {
            self.runtime.hold_back(&id);
        }
        self.runs.push(entry);
        self.note(&id);
        Ok(id)
    }

    /// The active runs of workflow `name` in `project`, in start order. Active is what holds a
    /// task ([`RunStatus::is_active`]): queued, running, pausing, paused, halted, waiting on you
    /// or failed.
    fn active_instances(&self, name: &str, project: Option<&str>) -> Vec<&RunEntry> {
        self.runs
            .iter()
            .filter(|entry| entry.record.workflow == name && entry.project.as_deref() == project)
            .filter(|entry| self.status(&entry.id).is_some_and(RunStatus::is_active))
            .collect()
    }

    /// The instance limit of a start: a definition with no `instances` has none. At the limit a
    /// hand start that asks to queue is let through held back (`Ok(true)`); any other start is
    /// refused naming an active run (one already let start first).
    fn check_instances(
        &self,
        name: &str,
        definition: &Definition,
        project: Option<&str>,
        request: &StartRequest,
    ) -> Result<bool, Refusal> {
        let Some(limit) = definition.instances else {
            return Ok(false);
        };
        let active = self.active_instances(name, project);
        if active.len() < limit as usize {
            return Ok(false);
        }
        let by_hand = request.started_by.counts_toward_run_limit();
        if by_hand && request.queue {
            return Ok(true);
        }
        let named = active
            .iter()
            .find(|entry| !self.runtime.is_held_back(&entry.id))
            .or(active.first());
        let (run, task) = named.map_or((String::new(), String::new()), |entry| {
            (worktrees::short_id(&entry.id), entry.task.slug.clone())
        });
        let mut refusal = Refusal::new(
            RefusalKind::InstanceLimit,
            format!(
                "Workflow '{name}' already runs {} of its {limit} allowed in this project: run \
                 {run} on task {task}.",
                active.len()
            ),
            if by_hand {
                "Queue this run to start when one ends, or cancel that run first."
            } else {
                "Start it again once a run of it ends."
            },
        );
        if by_hand && let Some(entry) = named {
            refusal.offers = vec![Offer::Queue, Offer::Cancel(entry.id.clone())];
        }
        Err(refusal)
    }

    /// Lets each held-back run start while its workflow runs fewer than its `instances` in its
    /// project, oldest first (run ids are UUID v7, so time-ordered). A halted held-back run keeps
    /// waiting until it is resumed. Whether any was let start.
    fn release_instances(&mut self) -> bool {
        let mut released = false;
        let mut waiting: Vec<RunId> = self
            .runs
            .iter()
            .filter(|entry| {
                self.runtime.is_held_back(&entry.id) && self.runtime.is_queued(&entry.id)
            })
            .map(|entry| entry.id.clone())
            .collect();
        waiting.sort();
        for id in waiting {
            let Ok(entry) = self.entry(&id) else {
                continue;
            };
            let limit = self
                .runtime
                .definition(&id)
                .and_then(|definition| definition.instances);
            let running = self
                .active_instances(&entry.record.workflow, entry.project.as_deref())
                .iter()
                .filter(|other| !self.runtime.is_held_back(&other.id))
                .count();
            if limit.is_none_or(|limit| running < limit as usize) {
                self.runtime.let_start(&id);
                released = true;
            }
        }
        released
    }

    /// The checks of a request's own fields, before anything is read: a tree this version cannot
    /// make yet (a lane's tree for a run with no task or with a second tree, start or merge
    /// target; a snapshot or a named repository for a task run), a lane's tree or lane
    /// directories with no lane, a base or merge target with no repository, a reserved run id that is not a UUID v7 or is taken, and a definition commit the
    /// caller did not read the layers from. A request that sets none of these passes.
    fn check_request(
        &self,
        request: &StartRequest,
        context: &StartContext<'_>,
    ) -> Result<(), Refusal> {
        match (&request.tree, &request.subject) {
            (
                RunTree::Fresh {
                    repo: None,
                    base: None,
                    merge_into: None,
                },
                _,
            )
            | (RunTree::Fresh { repo: Some(_), .. }, StartSubject::Project { .. })
            | (RunTree::Snapshot { .. }, StartSubject::Project { .. }) => {}
            (RunTree::Fresh { repo: None, .. }, StartSubject::Project { .. }) => {
                return Err(Refusal::new(
                    RefusalKind::BadRequest,
                    "The run names a base or a branch to merge into, but no repository.",
                    "Name the repository too, or neither.",
                ));
            }
            (RunTree::Fresh { .. }, StartSubject::Task { .. }) => {
                return Err(unsupported(
                    "A task run makes its worktree from the task's own repository and branch.",
                ));
            }
            (
                RunTree::Lane {
                    base: None,
                    start: None,
                    merge_into: None,
                    ..
                },
                StartSubject::Task { .. },
            ) => {
                if request.lane.is_none() {
                    return Err(Refusal::new(
                        RefusalKind::BadRequest,
                        "The run names a lane's tree but no lane.",
                        "Name the lane, or no lane tree.",
                    ));
                }
            }
            (RunTree::Lane { .. }, _) => {
                return Err(unsupported(
                    "Only a task run may run in a lane's tree, with no second tree, start or \
                     merge target yet.",
                ));
            }
            (RunTree::Snapshot { .. }, StartSubject::Task { .. }) => {
                return Err(unsupported(
                    "A task run works in its own run worktree, not on a snapshot.",
                ));
            }
        }
        if !request.lane_dirs.is_empty() && request.lane.is_none() {
            return Err(Refusal::new(
                RefusalKind::BadRequest,
                "The run names lane directories but no lane.",
                "Name the lane, or no directories.",
            ));
        }
        if let Some(id) = &request.run_id {
            if !is_uuid_v7(id) {
                return Err(Refusal::new(
                    RefusalKind::BadRequest,
                    format!("The run id '{id}' is not a UUID v7."),
                    "Reserve a new run id.",
                ));
            }
            if self.runs.iter().any(|entry| &entry.id == id) {
                return Err(Refusal::new(
                    RefusalKind::BadRequest,
                    format!("A run {id} already exists."),
                    "Reserve a new run id.",
                ));
            }
        }
        if let Some(commit) = &request.definition_commit
            && context.definition_commit.as_deref() != Some(commit.0.as_str())
        {
            return Err(Refusal::new(
                RefusalKind::BadRequest,
                format!(
                    "The run asks for the definitions of commit {}, which were not read.",
                    commit.0
                ),
                "Start it from a project whose repository has that commit.",
            ));
        }
        Ok(())
    }

    /// A start with a project subject: the project must be loaded, the workflow named, and every
    /// input given in `overrides` or `literal:`; a workflow with a `task.*` or `todo.*` input is
    /// refused naming it. Then the workflow's `instances`, and the tree:
    /// - `RunTree::Fresh` with a repository: a run worktree of that repository on a new run branch
    ///   ([`worktrees::run_branch_name`]) from `base`, by default the branch its main checkout has
    ///   out. The success end merges the run branch into `merge_into` when it is set
    ///   ([`crate::builtin_merge::merge_project_run`]), never before.
    /// - `RunTree::Snapshot`: the nodes run in the snapshot, which no program may write; there is
    ///   no run branch and nothing is merged.
    /// - `RunTree::Fresh` with no repository: no worktree and no branch; the nodes run in the
    ///   run's work directory ([`project_work_dir`]), the only part of the project folder its
    ///   programs may write, and nothing is merged.
    ///
    /// The record is written under the
    /// project's own folder, `runs/<id>`; the agents are pointed at the snapshot, else at
    /// [`StartContext::definitions`].
    fn start_project(
        &mut self,
        request: StartRequest,
        project_id: &str,
        dirs: &LayerDirs,
        context: &StartContext<'_>,
    ) -> Result<RunId, Refusal> {
        let tree = context.tree;
        let Some(project) = tree
            .projects
            .iter()
            .find(|project| project.task.id() == Some(project_id))
        else {
            return Err(Refusal::new(
                RefusalKind::UnknownProject,
                format!("No project {project_id} is loaded."),
                "Pick a project in the Work view.",
            ));
        };
        self.refuse_moving(&project.task.dir)?;
        let name = request.workflow.clone().ok_or_else(|| {
            Refusal::new(
                RefusalKind::UnknownWorkflow,
                "A run with no task has no todo to name its workflow.",
                "Name the workflow when starting.",
            )
        })?;
        let (resolved, definition) = load_definition(
            dirs,
            &name,
            context.checks.validation(),
            Some(context.checks),
        )?;
        // A dispatcher is refused on a task but starts on a project: the lanes' dispatcher run
        // is a project run.
        if let Some(problem) = project_subject_problems(&definition).into_iter().next() {
            return Err(Refusal::new(
                RefusalKind::NeedsTask,
                problem.message,
                "Start it on a task, or pick a workflow that does not read one.",
            ));
        }
        let inputs = start_inputs(&name, &definition, None, tree, None, &request.overrides)?;
        check_machine(&definition, context.checks)?;
        let slug = project.slug.clone();
        let held_back = self.check_instances(&name, &definition, Some(&slug), &request)?;

        let project_dir = project.task.dir.clone();
        let id = request.run_id.clone().unwrap_or_else(new_id);
        let (checkout, worktree, record_tree) = match &request.tree {
            RunTree::Snapshot { path } => {
                if !path.is_dir() {
                    return Err(Refusal::new(
                        RefusalKind::BadRequest,
                        format!("The snapshot {} is not a directory.", path.display()),
                        "Materialize the snapshot first.",
                    ));
                }
                let scratch = project_work_dir(&project_dir, &id);
                let writable = [project_dir.as_path(), scratch.as_path()]
                    .into_iter()
                    .chain(request.lane_dirs.values().map(PathBuf::as_path));
                if let Some(shared) = snapshot_overlap(path, writable) {
                    return Err(Refusal::new(
                        RefusalKind::BadRequest,
                        format!(
                            "The snapshot {} overlaps {}, which the run may write.",
                            path.display(),
                            shared.display()
                        ),
                        "Materialize the snapshot outside the project folder and the lane directories.",
                    ));
                }
                let checkout = TaskCheckout {
                    slug: slug.clone(),
                    worktree: None,
                    branch: None,
                };
                let worktree = RunWorktree {
                    run_dir: scratch,
                    path: path.clone(),
                    branch: String::new(),
                    // No repository of the run's: nothing for host git to name.
                    git_dir: PathBuf::new(),
                };
                let record = RunTreeRecord::Snapshot { path: path.clone() };
                (checkout, worktree, record)
            }
            RunTree::Fresh {
                repo: Some(repo),
                base,
                merge_into,
            } => {
                let base = match base.clone().or_else(|| self.worktrees.base_branch(repo)) {
                    Some(base) => base,
                    None => {
                        return Err(Refusal::new(
                            RefusalKind::BadRequest,
                            format!(
                                "The repository {} has no branch checked out to start from.",
                                repo.display()
                            ),
                            "Name the base branch.",
                        ));
                    }
                };
                let checkout = TaskCheckout {
                    slug: slug.clone(),
                    worktree: Some(repo.clone()),
                    branch: Some(base.clone()),
                };
                let worktree = self.create_project_worktree(&checkout, &id)?;
                let record = RunTreeRecord::Repository {
                    repo: repo.clone(),
                    base,
                    merge_into: merge_into.clone(),
                };
                (checkout, worktree, record)
            }
            RunTree::Fresh {
                repo: None,
                base: None,
                merge_into: None,
            } => {
                let work = project_work_dir(&project_dir, &id);
                // The work directory is the only part of the project folder the run may write:
                // a lane directory may not share files with the rest of it.
                let folder = crate::lifecycle::resolved(&project_dir);
                let own = crate::lifecycle::resolved(&work);
                if let Some(shared) = request.lane_dirs.values().find(|dir| {
                    let dir = crate::lifecycle::resolved(dir);
                    folder.starts_with(&dir) || (dir.starts_with(&folder) && !dir.starts_with(&own))
                }) {
                    return Err(Refusal::new(
                        RefusalKind::BadRequest,
                        format!(
                            "The lane directory {} overlaps the project folder {}, which holds \
                             the project's runs.",
                            shared.display(),
                            project_dir.display()
                        ),
                        "Keep the lane directories outside the project folder.",
                    ));
                }
                let work = create_project_work_dir(&project_dir, &id).map_err(|error| {
                    Refusal::new(
                        RefusalKind::BadRequest,
                        format!(
                            "The work directory {} cannot be made: {error}.",
                            work.display()
                        ),
                        "Make the project's work folder a plain directory.",
                    )
                })?;
                let checkout = TaskCheckout {
                    slug: slug.clone(),
                    worktree: None,
                    branch: None,
                };
                let worktree = RunWorktree {
                    run_dir: work.clone(),
                    path: work,
                    branch: String::new(),
                    git_dir: PathBuf::new(),
                };
                (checkout, worktree, RunTreeRecord::Work)
            }
            RunTree::Fresh { repo: None, .. } | RunTree::Lane { .. } => {
                return Err(unsupported(
                    "A run with no task cannot be made in this tree.",
                ));
            }
        };
        let definitions = match &record_tree {
            RunTreeRecord::Snapshot { path } => path.clone(),
            RunTreeRecord::Repository { .. } | RunTreeRecord::Work | RunTreeRecord::Lane { .. } => {
                context
                    .definitions
                    .clone()
                    .unwrap_or_else(|| project_dir.clone())
            }
        };
        let dir = project_dir.join(RUNS_DIR).join(&id);
        let created = now().to_string();
        let subject = RunSubject::Project {
            id: project_id.to_string(),
        };
        let record = RunToml {
            schema: RUN_SCHEMA,
            id: id.clone(),
            workflow: name,
            layer: resolved.layer.label().to_string(),
            task_id: subject.task_id_mirror(),
            subject,
            created: created.clone(),
            started_by: request.started_by,
            parent: request.parent.clone(),
            lane: request.lane.clone(),
            unattended: request.unattended,
            tree: Some(record_tree),
            definitions: Some(definitions),
            env: request.env.clone(),
            lane_dirs: request.lane_dirs,
            todo: None,
            inputs: inputs
                .into_iter()
                .map(|input| RecordedInput {
                    name: input.name,
                    source: input.source,
                    value: input.value,
                })
                .collect(),
            attempts: Vec::new(),
        };
        let setup = setup_of(&record, tree, &project_dir);
        let mut entry = RunEntry {
            id: id.clone(),
            task_id: String::new(),
            project: Some(slug),
            task: checkout,
            dir,
            record,
            worktree,
            rev: 0,
            setup,
        };
        let commit = context
            .definition_commit
            .clone()
            .filter(|_| resolved.layer == Layer::Project);
        if let Err(refusal) =
            self.begin_attempt(&mut entry, 1, &resolved.dir, definition, created, commit)
        {
            // The run never began: its empty work directory goes with it.
            if entry.setup.work_dir {
                remove_project_work_dir(&project_dir, &id).ok();
            }
            return Err(refusal);
        }
        if held_back {
            self.runtime.hold_back(&id);
        }
        self.runs.push(entry);
        self.note(&id);
        Ok(id)
    }

    /// The run worktree of a project run on a repository, with the refusals named for a
    /// repository rather than a task.
    fn create_project_worktree(
        &mut self,
        checkout: &TaskCheckout,
        run: &str,
    ) -> Result<RunWorktree, Refusal> {
        let repo = checkout
            .worktree
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        self.worktrees
            .create(checkout, run, 1, &self.settings)
            .map_err(|error| match error {
                WorktreeError::NotStarted => Refusal::new(
                    RefusalKind::BadRequest,
                    format!("{repo} is not a repository directory."),
                    "Name a repository this machine has.",
                ),
                WorktreeError::LowDiskSpace { .. } => Refusal::new(
                    RefusalKind::LowDiskSpace,
                    format!("Not enough free space: {error}."),
                    "Free some space, or lower workflows.min_free_disk_mb.",
                ),
                other => Refusal::io(other),
            })
    }

    /// Loads the runs a relaunch finds: `runs/<id>/run.toml` under every task directory of `tree`.
    /// A run whose directory holds the `cleared` marker, whose `run.toml` does not parse, that has
    /// no `events.jsonl` (or one that cannot be read or parsed), whose id repeats one already
    /// read in this batch, or whose task directory or record cannot be read is logged and skipped;
    /// the others still load. The definition revision comes from the log's last `DefinitionReloaded`. All loaded runs go to [`Runtime::load_all`] in one call, so orphan
    /// processes die in one grace period and a run with a started key and no finish comes back
    /// interrupted; each is bound to its attempt's worktree. Nothing resumes by itself. With no
    /// `runs/` directory this loads nothing and writes nothing. Takes no lock. A run already
    /// loaded is [`RuntimeError::Duplicate`] and none of these load; a log that cannot be opened
    /// skips only its run.
    pub fn load_runs(&mut self, tree: &WorkTree) -> Result<LoadReport, RuntimeError> {
        self.load_runs_where(tree, None, &[])
    }

    /// [`Self::load_runs`] of every task outside `skip_under` (the files folders of projects whose
    /// writer another process holds: their runs are that process's, and loading them here would
    /// stop its running nodes and write to its logs).
    pub fn load_runs_except(
        &mut self,
        tree: &WorkTree,
        skip_under: &[PathBuf],
    ) -> Result<LoadReport, RuntimeError> {
        self.load_runs_where(tree, None, skip_under)
    }

    /// [`Self::load_runs`] of only `runs`, those of them not loaded yet: what a take-back after a
    /// failed handover loads again (the runs that handover moved), beside the runs that never
    /// left. Runs another handover moved to another process stay there.
    pub fn load_new_runs(
        &mut self,
        tree: &WorkTree,
        runs: &[RunId],
    ) -> Result<LoadReport, RuntimeError> {
        self.load_runs_where(tree, Some(runs), &[])
    }

    fn load_runs_where(
        &mut self,
        tree: &WorkTree,
        only: Option<&[RunId]>,
        skip_under: &[PathBuf],
    ) -> Result<LoadReport, RuntimeError> {
        let skip_loaded = only.is_some();
        let mut report = LoadReport::default();
        let mut entries: Vec<RunEntry> = Vec::new();
        let mut specs: Vec<RunSpec> = Vec::new();
        let mut seen: std::collections::BTreeSet<RunId> = std::collections::BTreeSet::new();
        for task in tree.tasks() {
            if skip_under.iter().any(|files| task.dir.starts_with(files)) {
                continue;
            }
            let runs = bongos_work_model::layout::runs_dir(&task.dir);
            let listing = match fs::read_dir(&runs) {
                Ok(listing) => listing,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    skip(
                        &mut report,
                        &runs,
                        format!("cannot read the runs directory: {error}"),
                    );
                    continue;
                }
            };
            let mut dirs: Vec<PathBuf> = Vec::new();
            for entry in listing {
                match entry {
                    Ok(entry) if entry.path().is_dir() => dirs.push(entry.path()),
                    Ok(_) => {}
                    Err(error) => {
                        skip(&mut report, &runs, format!("cannot read an entry: {error}"))
                    }
                }
            }
            dirs.sort();
            let project = task.id().and_then(|id| project_of(tree, id));
            for dir in dirs {
                match self.read_run(tree, task, project.clone(), &dir) {
                    Ok(Some((entry, _))) if only.is_some_and(|only| !only.contains(&entry.id)) => {}
                    Ok(Some((entry, _)))
                        if skip_loaded && self.runs.iter().any(|run| run.id == entry.id) => {}
                    Ok(Some((entry, _))) if seen.contains(&entry.id) => skip(
                        &mut report,
                        &dir,
                        format!(
                            "run {} is already loaded from another task directory",
                            entry.id
                        ),
                    ),
                    Ok(Some((entry, spec))) => {
                        seen.insert(entry.id.clone());
                        entries.push(entry);
                        specs.push(spec);
                    }
                    Ok(None) => {}
                    Err(reason) => skip(&mut report, &dir, reason),
                }
            }
        }
        let orphans = self.runtime.load_all(specs)?;
        for (entry, orphans) in entries.into_iter().zip(orphans) {
            let orphans = match orphans {
                Ok(orphans) => orphans,
                Err(error) => {
                    skip(&mut report, &entry.dir, error.to_string());
                    continue;
                }
            };
            self.runtime
                .set_started_by(&entry.id, entry.record.started_by);
            self.runtime.set_unattended(
                &entry.id,
                entry.record.unattended.is_some_and(|choice| choice.value),
            );
            self.runtime.bind(&entry.id, entry.binding());
            report.loaded.push(entry.id.clone());
            report.orphans.push((entry.id.clone(), orphans));
            self.runs.push(entry);
        }
        // A run this process drives now is no longer shown from its records.
        self.observed
            .retain(|observed| !report.loaded.contains(&observed.entry.id));
        // A run queued at its workflow's instance limit waits again: every active loaded run that
        // has not begun (queued, or halted before it began) is held back, then the queued ones
        // are let start while their workflow has room, oldest first.
        for id in &report.loaded {
            if self.limited_before_begun(id) && self.status(id).is_some_and(RunStatus::is_active) {
                self.runtime.hold_back(id);
            }
        }
        self.release_instances();
        self.note_all();
        Ok(report)
    }

    /// One run directory: `Ok(None)` for a cleared run.
    fn read_run(
        &self,
        tree: &WorkTree,
        task: &Task,
        project: Option<String>,
        dir: &Path,
    ) -> Result<Option<(RunEntry, RunSpec)>, String> {
        Ok(self
            .read_run_state(tree, task, project, dir)?
            .map(|(entry, spec, _, _)| (entry, spec)))
    }

    /// [`Self::read_run`] with the replayed log of the current attempt and the number of bytes
    /// of the log that were read. Only reads.
    fn read_run_state(
        &self,
        tree: &WorkTree,
        task: &Task,
        project: Option<String>,
        dir: &Path,
    ) -> Result<Option<(RunEntry, RunSpec, RunState, u64)>, String> {
        if dir.join(CLEARED_FILE).exists() {
            return Ok(None);
        }
        let file = dir.join(RUN_FILE);
        let text = fs::read_to_string(&file).map_err(|error| format!("{RUN_FILE}: {error}"))?;
        let record = RunToml::parse(&text).map_err(|error| error.to_string())?;
        if dir
            .file_name()
            .is_none_or(|name| name.to_string_lossy() != record.id)
        {
            return Err(format!(
                "{RUN_FILE} names run {}, not this directory",
                record.id
            ));
        }
        let attempt = record
            .current_attempt()
            .ok_or_else(|| format!("{RUN_FILE} has no attempt"))?;
        let attempt_dir = attempt_dir(dir, attempt.n);
        if !attempt_dir.join(crate::runtime::EVENTS_FILE).is_file() {
            return Err(format!(
                "no {} in attempt {}",
                crate::runtime::EVENTS_FILE,
                attempt.n
            ));
        }
        // The log is authoritative for the revision: a `definition/<n>` directory with no
        // `DefinitionReloaded` is a reload that did not finish. This parse only reads; the
        // runtime's own open repairs a cut tail later.
        let events_path = attempt_dir.join(crate::runtime::EVENTS_FILE);
        let bytes = fs::read(&events_path)
            .map_err(|error| format!("{}: {error}", crate::runtime::EVENTS_FILE))?;
        let parsed =
            bongos_workflow_model::record::parse_log(&bytes).map_err(|error| error.to_string())?;
        let state = bongos_workflow_model::record::replay(&parsed.events);
        let rev = state.definition_rev.unwrap_or(0);
        let definition = read_definition(&definition_copy_dir(&attempt_dir, rev))?;
        let checkout = match (&record.tree, &task.file) {
            (Some(RunTreeRecord::Repository { repo, base, .. }), _) => TaskCheckout {
                slug: task.slug.clone(),
                worktree: Some(repo.clone()),
                branch: Some(base.clone()),
            },
            (Some(RunTreeRecord::Snapshot { .. } | RunTreeRecord::Work), _)
            | (None | Some(RunTreeRecord::Lane { .. }), None) => TaskCheckout {
                slug: task.slug.clone(),
                worktree: None,
                branch: None,
            },
            (None | Some(RunTreeRecord::Lane { .. }), Some(file)) => {
                TaskCheckout::from_task(task.slug.clone(), file)
            }
        };
        let worktree = match &record.tree {
            Some(RunTreeRecord::Snapshot { .. }) => RunWorktree {
                // The snapshot is no run directory: the run's own directory is its work directory.
                // It is no repository of the run's, so no git directory is recorded or looked up.
                run_dir: project_work_dir(&task.dir, &record.id),
                path: PathBuf::from(&attempt.worktree),
                branch: attempt.branch.clone(),
                git_dir: PathBuf::new(),
            },
            Some(RunTreeRecord::Work) => {
                // The work directory is where the project folder has it, whatever the record says.
                let work = project_work_dir(&task.dir, &record.id);
                RunWorktree {
                    run_dir: work.clone(),
                    path: work,
                    branch: String::new(),
                    git_dir: PathBuf::new(),
                }
            }
            _ => {
                let mut worktree = attempt_worktree(attempt, &checkout)?;
                // The attempt that ran in the lane's tree keeps its own run directory beside it.
                if let Some(RunTreeRecord::Lane { path, run_dir }) = &record.tree
                    && *path == worktree.path
                {
                    worktree.run_dir = run_dir.clone();
                }
                worktree
            }
        };
        let setup = setup_of(&record, tree, &task.dir);
        let task_id = record.task_id.clone();
        let spec = RunSpec {
            id: record.id.clone(),
            definition,
            attempt_dir,
            loop_bound: attempt
                .loop_bound
                .unwrap_or(self.settings.limits.loop_bound),
        };
        let entry = RunEntry {
            id: record.id.clone(),
            task_id,
            project,
            task: checkout,
            dir: dir.to_path_buf(),
            record,
            worktree,
            rev,
            setup,
        };
        Ok(Some((entry, spec, state, bytes.len() as u64)))
    }

    /// `workflow: clear finished`, the first half: every run that ended succeeded or was
    /// cancelled leaves the lifecycle. A run with no repository has its work directory removed
    /// first ([`remove_project_work_dir`], nothing outside it). Its `cleared` marker is written
    /// (and synced), then the run is dropped from the runtime; a run whose work directory or
    /// marker cannot be written stays listed and is reported in [`ClearOutcome::failed`]. A failed run counts as active and stays, and so does
    /// every other active run: the status is read again here, so a run that became active again
    /// is never cleared. A second call finds nothing to do. The caller holds the lifecycle lock,
    /// which serializes this against a tick or a command. Nothing is written when no run is
    /// finished.
    pub fn clear_finished(&mut self) -> ClearOutcome {
        let finished: Vec<RunId> = self
            .runs
            .iter()
            .filter(|entry| {
                self.status(&entry.id)
                    .is_some_and(|status| !status.is_active())
            })
            .map(|entry| entry.id.clone())
            .collect();
        let mut outcome = ClearOutcome::default();
        for id in finished {
            let Some(index) = self.runs.iter().position(|entry| entry.id == id) else {
                continue;
            };
            if self.status(&id).is_none_or(RunStatus::is_active) {
                continue;
            }
            // A run with no repository takes its work directory with it, before its marker, so
            // a directory that cannot be removed leaves the run listed to clear again.
            if self.runs[index].setup.work_dir {
                let entry = &self.runs[index];
                let removed = entry
                    .dir
                    .parent()
                    .filter(|runs| runs.file_name().is_some_and(|name| name == RUNS_DIR))
                    .and_then(Path::parent)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("{} is not in a runs directory", entry.dir.display()),
                        )
                    })
                    .and_then(|project_dir| remove_project_work_dir(project_dir, &entry.id));
                if let Err(error) = removed {
                    log::error!(
                        "workflows: could not remove the work directory of run {id}: {error}"
                    );
                    outcome.failed.push((id, error.to_string()));
                    continue;
                }
            }
            let marker = self.runs[index].dir.join(CLEARED_FILE);
            if let Err(error) = fs::File::create(&marker).and_then(|file| file.sync_all()) {
                log::error!(
                    "workflows: could not mark run {id} cleared ({}): {error}",
                    marker.display()
                );
                outcome.failed.push((id, error.to_string()));
                continue;
            }
            self.runtime.remove(&id);
            self.runs.remove(index);
            self.statuses.remove(&id);
            outcome.cleared.push(id);
        }
        outcome
    }

    /// The output fields of the held result of `key`, for `workflow: approve` with an edit: the
    /// fields its node declares that the result has, in declaration order. Empty when the node or
    /// its result is not there.
    pub fn gate_fields(&self, run: &str, key: &NodeKey) -> Vec<GateField> {
        let Some(state) = self.state(run) else {
            return Vec::new();
        };
        let Some(node) = self
            .runtime
            .definition(run)
            .and_then(|definition| definition.node(&key.node))
        else {
            return Vec::new();
        };
        let Some(Value::Object(result)) = state
            .last_finish(key)
            .and_then(|finished| finished.result.clone())
        else {
            return Vec::new();
        };
        node.output
            .iter()
            .filter_map(|field| {
                Some(GateField {
                    name: field.name.clone(),
                    field_type: field.field_type.clone(),
                    value: result.get(&field.name)?.clone(),
                })
            })
            .collect()
    }

    /// The text of `workflow: dump engine`: every loaded run with its id, status, keys in flight,
    /// queue position and cost against the cap, then the tries in flight per kind of node.
    pub fn dump_text(&self) -> String {
        use std::fmt::Write as _;
        let cap = self.runtime.limits().run_max_cost_usd;
        let mut text = String::from("workflow engine\n");
        let _ = writeln!(text, "runs: {}", self.runs.len());
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &self.runs {
            let state = self.state(&entry.id);
            let status = state
                .as_ref()
                .map_or("unknown", |state| RunStatus::of(state).label());
            let cost = state.as_ref().map_or(0.0, |state| state.cost_usd);
            let queue = match self.runtime.queue_position(&entry.id) {
                Some(position) => format!("queue position {position}"),
                None => "not queued".to_string(),
            };
            let cap_text = match cap {
                Some(cap) => format!("cap ${cap:.2}"),
                None => "no cap".to_string(),
            };
            let in_flight = self.runtime.in_flight(&entry.id);
            let keys = if in_flight.is_empty() {
                "none".to_string()
            } else {
                in_flight
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let _ = writeln!(
                text,
                "run {} task {} workflow {}: {status}; in flight: {keys}; {queue}; cost ${cost:.2}, {cap_text}",
                entry.id, entry.task_id, entry.record.workflow
            );
            let definition = self.runtime.definition(&entry.id);
            for key in &in_flight {
                let kind = definition
                    .and_then(|definition| {
                        definition.node(&key.node).map(|node| (definition, node))
                    })
                    .map_or_else(
                        || "unknown".to_string(),
                        |(definition, node)| match node.kind.value {
                            NodeKind::Agent => {
                                format!("agent ({:?})", agent_of(definition, node)).to_lowercase()
                            }
                            NodeKind::Program => match &node.use_node {
                                Some(name) => format!("built-in {}", name.value),
                                None => "program".to_string(),
                            },
                        },
                    );
                *kinds.entry(kind).or_default() += 1;
            }
        }
        if kinds.is_empty() {
            text.push_str("tries in flight: none\n");
        } else {
            text.push_str("tries in flight by kind:\n");
            for (kind, count) in &kinds {
                let _ = writeln!(text, "  {kind}: {count}");
            }
        }
        text
    }

    /// Quit: stops every process of every run (within 5 s) and records `Interrupted` with the
    /// keys in flight. Nothing starts afterwards.
    pub fn quit(&mut self) {
        self.runtime.quit();
        self.note_all();
    }

    /// Workflows turned off: stops every process of every run and records `Interrupted` with the
    /// keys in flight ([`Runtime::interrupt_all`]). Later starts still work. Whether any run was
    /// interrupted.
    pub fn interrupt_all(&mut self) -> bool {
        let changed = self.runtime.interrupt_all();
        if changed {
            self.note_all();
        }
        changed
    }

    /// "Interrupt them now" at a handover: the processes of `runs` stop and each run with a node
    /// in flight gets `Interrupted` with those keys, as at quit. The other runs go on. Whether
    /// any run was interrupted.
    pub fn interrupt_runs(&mut self, runs: &[RunId]) -> bool {
        let changed = self.runtime.interrupt_runs(runs);
        if changed {
            self.note_all();
        }
        changed
    }

    /// The handover of `runs` to the process `to_pid`: `HandedOver { to_pid }` to each that is
    /// not over, then each leaves this lifecycle without another event
    /// ([`Self::unload`]), so nothing here writes to them again, a later quit included.
    pub fn hand_over_runs(&mut self, to_pid: u32, runs: &[RunId]) {
        self.runtime.hand_over_runs(to_pid, runs);
        for run in runs {
            self.unload(run);
        }
    }

    /// Drops `run` from this lifecycle and its runtime without appending any event or stopping
    /// anything: it belongs to another process now. Whether it was loaded.
    pub fn unload(&mut self, run: &str) -> bool {
        let Some(index) = self.runs.iter().position(|entry| entry.id == run) else {
            return false;
        };
        self.runtime.unload(run);
        self.runs.remove(index);
        self.statuses.remove(run);
        true
    }

    /// The runs whose directory is under `dir` (a project's files), in start order.
    pub fn runs_under(&self, dir: &Path) -> Vec<RunId> {
        self.runs
            .iter()
            .filter(|entry| entry.dir.starts_with(dir))
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Pauses `run`: no new node starts, the running ones finish, then it reads Paused.
    pub fn pause(&mut self, run: &str) -> Result<(), Refusal> {
        let status = self.status_of(run)?;
        if !matches!(
            status,
            RunStatus::Queued | RunStatus::Running | RunStatus::WaitingOnYou
        ) {
            return Err(wrong_state(status, "pause"));
        }
        self.runtime.pause(run).map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// Halts `run`: stops every running process (within 5 s) and marks it Halted. The stopped
    /// keys rerun from their start on resume.
    pub fn halt(&mut self, run: &str) -> Result<(), Refusal> {
        let status = self.status_of(run)?;
        if !status.is_active() || matches!(status, RunStatus::Halted | RunStatus::Failed) {
            return Err(wrong_state(status, "halt"));
        }
        self.runtime.halt(run).map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// The run worktree of `run` and its fan-out branch worktrees (`b-*` in the attempt's
    /// directory), for editing a halted run by hand; none for a run on a snapshot.
    pub fn halt_worktrees(&self, run: &str) -> Option<HaltWorktrees> {
        let entry = self.entry(run).ok()?;
        // A snapshot is read-only to its run: there is nothing to edit by hand.
        if entry.setup.snapshot {
            return None;
        }
        // A work directory has no branches; its run directory is the tree itself.
        if entry.setup.work_dir {
            return Some(HaltWorktrees {
                run: entry.worktree.path.clone(),
                branches: Vec::new(),
            });
        }
        let mut branches: Vec<PathBuf> = fs::read_dir(&entry.worktree.run_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|item| item.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("b-"))
            })
            .collect();
        branches.sort();
        Some(HaltWorktrees {
            run: entry.worktree.path.clone(),
            branches,
        })
    }

    /// Resumes a paused, halted, interrupted or held run. Each key a halt stopped, a quit
    /// interrupted or a check without a verdict held reruns from its start in the same worktree,
    /// as a new try that does not count against `retries`; completed keys are not rerun.
    pub fn resume(&mut self, run: &str) -> Result<(), Refusal> {
        let state = self.entry(run).and_then(|_| self.state_of(run))?;
        let status = RunStatus::of(&state);
        let resumable =
            state.paused || state.halted || !state.interrupted.is_empty() || !state.held.is_empty();
        if !resumable || !status.is_active() {
            return Err(wrong_state(status, "resume"));
        }
        // A run that has not begun waits for its workflow's instance limit like a new start.
        if self.limited_before_begun(run) {
            self.runtime.hold_back(run);
        }
        self.runtime.resume(run).map_err(Refusal::runtime)?;
        self.release_instances();
        self.note(run);
        Ok(())
    }

    /// Whether run `run` has not begun and its definition limits its `instances`.
    fn limited_before_begun(&self, run: &str) -> bool {
        self.runtime
            .definition(run)
            .is_some_and(|definition| definition.instances.is_some())
            && !self.runtime.has_begun(run)
    }

    /// Cancels `run`: stops every process within 5 s and marks it Cancelled. On a Failed run it
    /// closes it out.
    pub fn cancel(&mut self, run: &str) -> Result<(), Refusal> {
        let status = self.status_of(run)?;
        if !status.is_active() {
            return Err(wrong_state(status, "cancel"));
        }
        self.runtime.cancel(run).map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// Reruns the failed key of a failed run, reusing every other result. `node` picks one when
    /// several failed; with none and several failed it is refused, listing them. With
    /// `fresh_definition` the latest definition is first copied to a new `definition/<rev>/` and
    /// the run continues on it, refused when it would lose a completed result. `commit` is the
    /// commit the project layer of `dirs` was read from; a fresh retry records it on the
    /// attempt, a retry without it keeps the attempt's own.
    pub fn retry(
        &mut self,
        run: &str,
        node: Option<NodeKey>,
        fresh_definition: bool,
        dirs: &LayerDirs,
        commit: Option<String>,
    ) -> Result<(), Refusal> {
        let state = self.entry(run).and_then(|_| self.state_of(run))?;
        let status = RunStatus::of(&state);
        if status != RunStatus::Failed {
            return Err(wrong_state(status, "retry a step of"));
        }
        let definition = self
            .runtime
            .definition(run)
            .cloned()
            .ok_or_else(|| unknown_run(run))?;
        let failed = retry_set(&definition, &state, &self.runtime.run_limits(run));
        let key = match node {
            Some(key) if failed.contains(&key) => key,
            Some(key) => {
                return Err(Refusal::new(
                    RefusalKind::WrongState,
                    format!("The step {key} did not fail."),
                    format!("Retry one of: {}.", key_list(&failed)),
                ));
            }
            None => match failed.as_slice() {
                [] => {
                    return Err(Refusal::new(
                        RefusalKind::WrongState,
                        "No step of this run can be retried.",
                        "Restart it instead (workflow: restart).",
                    ));
                }
                [only] => only.clone(),
                several => {
                    return Err(Refusal::new(
                        RefusalKind::SeveralFailed,
                        format!("Several steps failed: {}.", key_list(several)),
                        "Pick the one to retry.",
                    ));
                }
            },
        };
        if fresh_definition {
            self.reload_definition(run, &definition, &state, dirs, commit)?;
        }
        self.runtime.rerun(run, key).map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// Starts `run` over as a new attempt: the latest definition, a fresh run worktree and
    /// branch, the same todo and inputs, and no earlier results. An attempt still active is
    /// cancelled first; earlier attempts stay on disk. The run keeps its id and its one record.
    /// `commit` is the commit the project layer of `dirs` was read from; the new attempt records
    /// it when the definition comes from that layer.
    pub fn restart(
        &mut self,
        run: &str,
        dirs: &LayerDirs,
        commit: Option<String>,
    ) -> Result<RunId, Refusal> {
        let index = self.index(run)?;
        self.refuse_moving(&self.runs[index].dir)?;
        let status = self.status_of(run)?;
        if status == RunStatus::Succeeded {
            return Err(wrong_state(status, "restart"));
        }
        if !status.is_active() {
            let task = self.runs[index].task_id.clone();
            // A run with no task holds no task, so no other run refuses it.
            if !task.is_empty() && self.active_run_id(&task).is_some() {
                if let Some(refusal) = self.active_refusal(&task, None) {
                    return Err(refusal);
                }
            }
        }
        let name = self.runs[index].record.workflow.clone();
        let validation = self.settings.validation_context();
        let (resolved, definition) = load_definition(dirs, &name, validation, None)?;
        let attempt = self.runs[index].attempt() + 1;
        let checkout = self.runs[index].task.clone();
        // A snapshot is read, never made, and a work directory is the run's own: the new attempt
        // runs in the same one.
        let worktree = if self.runs[index].setup.branchless() {
            self.runs[index].worktree.clone()
        } else {
            self.create_worktree(&checkout, run, attempt)?
        };
        let old_definition = self.runtime.definition(run).cloned();
        let old_loop_bound = self.runtime.run_limits(run).loop_bound;
        let old_attempt_dir = self.runs[index].attempt_dir();
        if status.is_active() {
            self.runtime.cancel(run).map_err(Refusal::runtime)?;
        }
        self.runtime.remove(run);
        let mut entry = self.runs.remove(index);
        let (old_record, old_worktree, old_rev) =
            (entry.record.clone(), entry.worktree.clone(), entry.rev);
        entry.worktree = worktree;
        entry.record.layer = resolved.layer.label().to_string();
        let commit = commit.filter(|_| resolved.layer == Layer::Project);
        let result = self.begin_attempt(
            &mut entry,
            attempt,
            &resolved.dir,
            definition,
            now().to_string(),
            commit,
        );
        if result.is_err() {
            // Put the old attempt back so the run does not drop out.
            entry.record = old_record;
            entry.worktree = old_worktree;
            entry.rev = old_rev;
            self.runtime.remove(run);
            if let Some(definition) = old_definition {
                self.runtime
                    .add(RunSpec {
                        id: run.to_string(),
                        definition,
                        attempt_dir: old_attempt_dir,
                        loop_bound: old_loop_bound,
                    })
                    .ok();
                self.runtime.set_started_by(run, entry.record.started_by);
                self.runtime.set_unattended(
                    run,
                    entry.record.unattended.is_some_and(|choice| choice.value),
                );
                self.runtime.bind(run, entry.binding());
            }
        }
        self.runs.insert(index, entry);
        result?;
        self.note(run);
        Ok(run.to_string())
    }

    /// Approves the open gate of `key`. `edit` sets one output field of the held result first,
    /// checked against the node's output schema.
    pub fn approve(
        &mut self,
        run: &str,
        key: &NodeKey,
        edit: Option<(String, Value)>,
    ) -> Result<(), Refusal> {
        let state = self.open_gate(run, key)?;
        if edit.is_some() && state.person_only_gates.contains(key) {
            return Err(start_gate_as_is());
        }
        let edit = match edit {
            Some((field, value)) => Some(self.checked_edit(run, key, &state, field, value)?),
            None => None,
        };
        self.runtime
            .close_gate(run, key.clone(), BY_PERSON, edit, None)
            .map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// Sends the open gate of `key` back: the node reruns with `comment` appended to its prompt
    /// under "Revision requested:" with its previous result ([`Lifecycle::prompt_for`]).
    pub fn revise(&mut self, run: &str, key: &NodeKey, comment: String) -> Result<(), Refusal> {
        if self.open_gate(run, key)?.person_only_gates.contains(key) {
            return Err(start_gate_as_is());
        }
        let comment = comment.trim().to_string();
        if comment.is_empty() {
            return Err(Refusal::new(
                RefusalKind::WrongState,
                "A revision needs a comment.",
                "Say what to change.",
            ));
        }
        self.runtime
            .close_gate(run, key.clone(), BY_PERSON, None, Some(comment))
            .map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// Cancels the start gate of a built-in's `key`: the key fails with `GateCancelled`, never
    /// retried ([`Runtime::cancel_gate`]).
    pub fn cancel_gate(&mut self, run: &str, key: &NodeKey) -> Result<(), Refusal> {
        self.open_gate(run, key)?;
        self.runtime
            .cancel_gate(run, key.clone())
            .map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
    }

    /// Refuses closing the gate of `key` for a peer that cannot attest a person (`attested`
    /// false) when the gate is a built-in's start gate: only a person at a window approves it.
    pub fn refuse_unattested(
        &self,
        run: &str,
        key: &NodeKey,
        attested: bool,
    ) -> Result<(), Refusal> {
        refuse_unattested(&self.state_of(run)?, key, attested)
    }

    /// The prompt the next try of `key` gets from `base`: when the last decision on its gate was
    /// a revision, the comment and the previous result follow under "Revision requested:".
    pub fn prompt_for(&self, run: &str, key: &NodeKey, base: &str) -> String {
        match self.state(run) {
            Some(state) => prompt_with_revision(&state, key, base),
            None => base.to_string(),
        }
    }

    /// The shell command that resumes the agent session of `key` (or of the last node with a
    /// session) in its worktree: `claude --resume <id>` or `codex resume <id>`.
    pub fn resume_command(&self, run: &str, key: Option<&NodeKey>) -> Result<String, Refusal> {
        let entry = self.entry(run)?;
        let state = self.state_of(run)?;
        let definition = self
            .runtime
            .definition(run)
            .ok_or_else(|| unknown_run(run))?;
        let finished = state
            .finished
            .iter()
            .rev()
            .filter(|finished| key.is_none_or(|key| &finished.key == key))
            .find(|finished| finished.session_id.is_some())
            .ok_or_else(|| {
                Refusal::new(
                    RefusalKind::NoSession,
                    "No agent session of this run has been recorded yet.",
                    "Wait for an agent node to finish a try.",
                )
            })?;
        let session = finished.session_id.as_deref().unwrap_or_default();
        let kind = definition
            .node(&finished.key.node)
            .map(|node| agent_of(definition, node))
            .unwrap_or(AgentKind::Claude);
        // Every key of a run on a snapshot or in a work directory ran in that one tree.
        let directory = match finished.key.branch.last() {
            Some((fan, index)) if !entry.setup.branchless() => {
                branch_worktree_path(&entry.worktree.run_dir, fan, *index)
            }
            _ => entry.worktree.path.clone(),
        };
        let resume = match kind {
            AgentKind::Claude => format!("claude --resume {}", shell_quote(session)),
            AgentKind::Codex => format!("codex resume {}", shell_quote(session)),
        };
        Ok(format!(
            "cd {} && {resume}",
            shell_quote(&directory.to_string_lossy())
        ))
    }

    fn active_refusal(&self, task_id: &str, reported: Option<ActiveRun>) -> Option<Refusal> {
        let own = self.active_run_id(task_id);
        let active = reported.or_else(|| self.active_run(task_id))?;
        let at = if active.step.is_empty() {
            String::new()
        } else {
            format!(" at node {}", active.step)
        };
        let mut refusal = Refusal::new(
            RefusalKind::ActiveRun,
            format!(
                "This task already has an active workflow: {}, {}{at}. A task runs one \
                 workflow at a time.",
                active.workflow, active.status
            ),
            "Resume it, or cancel it first.",
        );
        if let Some(id) = own {
            refusal.offers = vec![Offer::Resume(id.clone()), Offer::Cancel(id)];
        }
        Some(refusal)
    }

    fn create_worktree(
        &mut self,
        checkout: &TaskCheckout,
        run: &str,
        attempt: u32,
    ) -> Result<RunWorktree, Refusal> {
        self.worktrees
            .create(checkout, run, attempt, &self.settings)
            .map_err(worktree_refusal)
    }

    /// The first attempt's run worktree in the lane's tree at `path`.
    fn lane_worktree(
        &mut self,
        checkout: &TaskCheckout,
        run: &str,
        path: &Path,
        common_dir: Option<&Path>,
    ) -> Result<RunWorktree, Refusal> {
        self.worktrees
            .lane(checkout, run, 1, path, common_dir, &self.settings)
            .map_err(worktree_refusal)
    }

    /// Writes attempt `n` of `entry` (the definition copy, `run.toml`) and hands it to the
    /// runtime.
    fn begin_attempt(
        &mut self,
        entry: &mut RunEntry,
        n: u32,
        source: &Path,
        definition: Definition,
        created: String,
        definition_commit: Option<String>,
    ) -> Result<(), Refusal> {
        let attempt_dir = attempt_dir(&entry.dir, n);
        let loop_bound = self.settings.limits.loop_bound;
        let copy = definition_copy_dir(&attempt_dir, 0);
        copy_dir(source, &copy).map_err(Refusal::io)?;
        entry.record.attempts.push(Attempt {
            n,
            created,
            definition_hash: hash_dir(&copy),
            definition_commit,
            worktree: entry.worktree.path.to_string_lossy().into_owned(),
            branch: entry.worktree.branch.clone(),
            git_dir: Some(entry.worktree.git_dir.to_string_lossy().into_owned())
                .filter(|git_dir| !git_dir.is_empty()),
            loop_bound: Some(loop_bound),
        });
        entry.rev = 0;
        write_record(&entry.dir, &entry.record).map_err(Refusal::io)?;
        self.runtime
            .add(RunSpec {
                id: entry.id.clone(),
                definition,
                attempt_dir,
                loop_bound,
            })
            .map_err(Refusal::runtime)?;
        self.runtime
            .set_started_by(&entry.id, entry.record.started_by);
        self.runtime.set_unattended(
            &entry.id,
            entry.record.unattended.is_some_and(|choice| choice.value),
        );
        self.runtime.bind(&entry.id, entry.binding());
        Ok(())
    }

    /// Copies the latest definition to the next `definition/<rev>/` and continues the run on
    /// it; refused (and the copy removed) when it would lose a completed result.
    fn reload_definition(
        &mut self,
        run: &str,
        current: &Definition,
        state: &RunState,
        dirs: &LayerDirs,
        commit: Option<String>,
    ) -> Result<(), Refusal> {
        let index = self.index(run)?;
        let name = self.runs[index].record.workflow.clone();
        let validation = self.settings.validation_context();
        let (resolved, _) = load_definition(dirs, &name, validation, None)?;
        let rev = self.runs[index].rev + 1;
        let copy = definition_copy_dir(&self.runs[index].attempt_dir(), rev);
        // `rev` is one past the log's current revision, so a directory here is the leftover of a
        // reload that crashed before `DefinitionReloaded`; nothing references it.
        if copy.exists() {
            fs::remove_dir_all(&copy).map_err(Refusal::io)?;
        }
        copy_dir(&resolved.dir, &copy).map_err(Refusal::io)?;
        let fresh = read_definition(&copy).map_err(Refusal::io);
        let checked = fresh.and_then(|fresh| {
            check_fresh_retry(current, &fresh, state)
                .map(|()| fresh)
                .map_err(|refusal| {
                    Refusal::new(
                        RefusalKind::FreshDefinition,
                        refusal.to_string(),
                        "(workflow: restart)",
                    )
                })
        });
        let fresh = match checked {
            Ok(fresh) => fresh,
            Err(refusal) => {
                fs::remove_dir_all(&copy).ok();
                return Err(refusal);
            }
        };
        self.runtime
            .reload_definition(run, fresh, rev)
            .map_err(Refusal::runtime)?;
        self.runs[index].rev = rev;
        // The attempt now reads the definition of the commit it was just copied from; a layer
        // that is not a repository's leaves the attempt's own commit as it was.
        let entry = &mut self.runs[index];
        if let Some(commit) = commit.filter(|_| resolved.layer == Layer::Project) {
            let number = entry.attempt();
            if let Some(attempt) = entry
                .record
                .attempts
                .iter_mut()
                .find(|attempt| attempt.n == number)
            {
                attempt.definition_commit = Some(commit);
                write_record(&entry.dir, &entry.record).map_err(Refusal::io)?;
            }
        }
        Ok(())
    }

    fn checked_edit(
        &self,
        run: &str,
        key: &NodeKey,
        state: &RunState,
        field: String,
        value: Value,
    ) -> Result<Value, Refusal> {
        let definition = self
            .runtime
            .definition(run)
            .ok_or_else(|| unknown_run(run))?;
        let node = definition.node(&key.node).ok_or_else(|| unknown_run(run))?;
        if !node.output.iter().any(|output| output.name == field) {
            return Err(Refusal::new(
                RefusalKind::InvalidEdit,
                format!("Node '{}' has no output field '{field}'.", key.node),
                format!(
                    "Edit one of: {}.",
                    node.output
                        .iter()
                        .map(|output| output.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        let held = state
            .last_finish(key)
            .and_then(|finished| finished.result.clone());
        let Some(Value::Object(mut result)) = held else {
            return Err(Refusal::new(
                RefusalKind::InvalidEdit,
                format!("Node '{}' has no result to edit yet.", key.node),
                "Approve without an edit.",
            ));
        };
        result.insert(field.clone(), value.clone());
        schema::check(&node.output, &Value::Object(result)).map_err(|detail| {
            Refusal::new(
                RefusalKind::InvalidEdit,
                format!(
                    "The new '{field}' does not fit the output of '{}': {detail}.",
                    key.node
                ),
                "Give a value of the field's type.",
            )
        })?;
        let mut edit = serde_json::Map::new();
        edit.insert(field, value);
        Ok(Value::Object(edit))
    }

    fn open_gate(&self, run: &str, key: &NodeKey) -> Result<RunState, Refusal> {
        let state = self.entry(run).and_then(|_| self.state_of(run))?;
        if !state.open_gates.contains_key(key) {
            return Err(Refusal::new(
                RefusalKind::NoGate,
                format!("No gate is open at {key}."),
                "Act on the gate the run is waiting at.",
            ));
        }
        Ok(state)
    }

    fn index(&self, run: &str) -> Result<usize, Refusal> {
        self.runs
            .iter()
            .position(|entry| entry.id == run)
            .ok_or_else(|| unknown_run(run))
    }

    fn entry(&self, run: &str) -> Result<&RunEntry, Refusal> {
        self.index(run).map(|index| &self.runs[index])
    }

    fn state_of(&self, run: &str) -> Result<RunState, Refusal> {
        self.state(run).ok_or_else(|| unknown_run(run))
    }

    fn status_of(&self, run: &str) -> Result<RunStatus, Refusal> {
        self.entry(run)?;
        self.status(run).ok_or_else(|| unknown_run(run))
    }

    /// Queues a `Changed` event for `run` when its status or its set of open gates changed since
    /// the last one.
    fn note(&mut self, run: &str) {
        let (Some(state), Ok(entry)) = (self.state(run), self.entry(run)) else {
            return;
        };
        let status = RunStatus::of(&state);
        let gates: BTreeSet<NodeKey> = state.open_gates.keys().cloned().collect();
        let task_id = entry.task_id.clone();
        if self.statuses.get(run) != Some(&(status, gates.clone())) {
            self.statuses.insert(run.to_string(), (status, gates));
            self.events.push(WorkflowEvent::Changed {
                run: run.to_string(),
                task_id,
                state: status,
            });
        }
    }

    fn note_all(&mut self) {
        let ids: Vec<RunId> = self.runs.iter().map(|entry| entry.id.clone()).collect();
        for id in ids {
            self.note(&id);
        }
    }
}

/// The todo a run completes: `index` when it names an open todo, else with no index the first
/// open one.
pub(crate) fn open_todo(
    task: &Task,
    index: Option<usize>,
) -> Option<(usize, &bongos_work_model::Todo)> {
    let todos = &task.file.as_ref()?.todos;
    match index {
        Some(index) => todos
            .get(index)
            .filter(|todo| todo.state == TodoState::Open)
            .map(|todo| (index, todo)),
        None => todos
            .iter()
            .enumerate()
            .find(|(_, todo)| todo.state == TodoState::Open),
    }
}

fn unsupported(message: &str) -> Refusal {
    Refusal::new(RefusalKind::Unsupported, message, "")
}

/// The slug of the project whose tree holds task `task_id`; `None` for a one-off or an unknown
/// task.
fn project_of(tree: &WorkTree, task_id: &str) -> Option<String> {
    tree.projects
        .iter()
        .find(|project| {
            project
                .task
                .walk()
                .iter()
                .any(|task| task.id() == Some(task_id))
        })
        .map(|project| project.slug.clone())
}

/// A dispatcher is never started as a task run: it has no nodes for the scheduler to drive, and
/// it starts only as a project run (the lanes' dispatcher run), which does not call this.
fn refuse_dispatcher(name: &str, definition: &Definition) -> Result<(), Refusal> {
    if definition.dispatcher {
        return Err(Refusal::new(
            RefusalKind::Unsupported,
            format!("Workflow '{name}' is a dispatcher; it is not started as a run."),
            "Pick another workflow.",
        ));
    }
    Ok(())
}

/// The inputs of a start, resolved for `task` (none for a project subject): refused when one
/// reads a task the run does not have, or when a required one has no value.
fn start_inputs(
    name: &str,
    definition: &Definition,
    task: Option<&Task>,
    tree: &WorkTree,
    todo: Option<usize>,
    overrides: &BTreeMap<String, InputValue>,
) -> Result<Vec<bongos_workflow_model::inputs::Input>, Refusal> {
    let inputs = bongos_workflow_model::resolve(definition, task, tree, todo, overrides).map_err(
        |error| {
            Refusal::new(
                RefusalKind::NeedsTask,
                error.to_string(),
                "Start it on a task, or pick a workflow that does not read one.",
            )
        },
    )?;
    if let Some(missing) = inputs.iter().find(|input| input.missing) {
        let fix = if task.is_some() {
            "Fill it in on the task, or give it when starting."
        } else {
            "Give it when starting."
        };
        return Err(Refusal::new(
            RefusalKind::MissingInput,
            format!(
                "Workflow '{name}' needs the input '{}' (from {}), which has no value.",
                missing.name, missing.source
            ),
            fix,
        ));
    }
    Ok(inputs)
}

/// Resolves and checks the definition `name` against `base` (the settings' context, so
/// `programs.allow_network = false` refuses a network entry at every start, restart and fresh
/// retry): refused when unknown or when it has errors. With `checks` that know a CLI's MCP
/// servers, validation leaves that CLI's servers to the start's own MCP check, which comes later
/// in the refusal order; otherwise validation reports them.
fn load_definition(
    dirs: &LayerDirs,
    name: &str,
    base: ValidationContext,
    checks: Option<&dyn StartChecks>,
) -> Result<(bongos_workflow_model::Resolved, Definition), Refusal> {
    let resolved = definitions::find_definition(dirs, Some(name)).map_err(|error| match error {
        DefinitionError::NotFound(_) | DefinitionError::NoDefinitions => Refusal::new(
            RefusalKind::UnknownWorkflow,
            format!("No workflow is named '{name}'."),
            "Check the name, or create it (workflow: new).",
        ),
        other => Refusal::io(other),
    })?;
    let report = definitions::check_definition_in(&resolved, base, |definition, context| {
        let Some(checks) = checks else {
            return;
        };
        for node in &definition.nodes {
            let kind = agent_of(definition, node);
            if checks.mcp_servers(kind).is_none() {
                continue;
            }
            let tools = allowed_tools(definition, node);
            let servers = context.mcp_servers.entry(kind).or_default();
            servers.extend(
                mcp::allowed_servers(&tools)
                    .into_iter()
                    .map(|(server, _)| server),
            );
        }
    });
    let errors = report.error_count();
    if errors > 0 {
        let first = report.first_error();
        let plural = if errors == 1 { "error" } else { "errors" };
        return Err(Refusal::new(
            RefusalKind::Invalid,
            format!(
                "Workflow '{name}' has {errors} {plural}: {}.",
                first.unwrap_or_default()
            ),
            "Fix the definition (workflow: validate lists them).",
        ));
    }
    let definition = read_definition(&resolved.dir).map_err(Refusal::io)?;
    Ok((resolved, definition))
}

fn read_definition(dir: &Path) -> Result<Definition, String> {
    let file = dir.join(DEFINITION_FILE);
    let text = fs::read_to_string(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    parse(&text).map_err(|error| error.to_string())
}

/// The agent CLIs, the interpreter and the MCP servers the definition needs, in that order.
fn check_machine(definition: &Definition, checks: &dyn StartChecks) -> Result<(), Refusal> {
    let agents: Vec<(&Node, AgentKind)> = definition
        .nodes
        .iter()
        .filter(|node| node.kind.value == NodeKind::Agent)
        .map(|node| (node, agent_of(definition, node)))
        .collect();
    let mut checked = BTreeSet::new();
    for (node, kind) in &agents {
        if !checked.insert(*kind) {
            continue;
        }
        if let Err(fix) = checks.agent(*kind) {
            return Err(Refusal::new(
                RefusalKind::AgentUnavailable,
                format!(
                    "Node '{}' needs {}, which cannot run.",
                    node.name,
                    agent_label(*kind)
                ),
                fix,
            ));
        }
    }
    if let Some(node) = definition
        .nodes
        .iter()
        .find(|node| node.kind.value == NodeKind::Program && node.use_node.is_none())
        && let Err(fix) = checks.interpreter()
    {
        return Err(Refusal::new(
            RefusalKind::InterpreterMissing,
            format!(
                "Node '{}' needs a program interpreter, which is missing.",
                node.name
            ),
            fix,
        ));
    }
    let exclude = checks.mcp_exclude();
    for (node, kind) in &agents {
        let Some(installed) = checks.mcp_servers(*kind) else {
            continue;
        };
        let tools = allowed_tools(definition, node);
        if let Err(failure) = mcp::check_start(*kind, None, &tools, &installed, &exclude) {
            return Err(Refusal::new(
                RefusalKind::McpServerMissing,
                format!("Node '{}' cannot use an MCP server it allows.", node.name),
                failure.detail,
            ));
        }
    }
    Ok(())
}

/// The tools a node allows: its own, else `[defaults]`.
fn allowed_tools(definition: &Definition, node: &Node) -> Vec<String> {
    node.settings
        .allowed_tools
        .clone()
        .or_else(|| definition.defaults.allowed_tools.clone())
        .unwrap_or_default()
}

/// The agent a node runs: its own, then `[defaults]`, then Claude.
pub fn agent_of(definition: &Definition, node: &Node) -> AgentKind {
    node.settings
        .agent
        .or(definition.defaults.agent)
        .unwrap_or(AgentKind::Claude)
}

fn agent_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "Claude Code",
        AgentKind::Codex => "Codex",
    }
}

fn wrong_state(status: RunStatus, verb: &str) -> Refusal {
    Refusal::new(
        RefusalKind::WrongState,
        format!("Cannot {verb} a run that is {}.", status.label()),
        "",
    )
}

fn key_list(keys: &[NodeKey]) -> String {
    keys.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The nodes a run is at, for its chip: the running, interrupted and gated nodes; else the node
/// it failed at; else the last node that finished.
pub fn step_names(state: &RunState) -> String {
    let mut names: Vec<&str> = Vec::new();
    for key in state
        .running
        .keys()
        .chain(state.interrupted.keys())
        .chain(state.open_gates.keys())
    {
        if !names.contains(&key.node.as_str()) {
            names.push(&key.node);
        }
    }
    if names.is_empty() {
        let failed = state
            .ended
            .as_ref()
            .and_then(|(_, failure)| failure.as_ref())
            .and_then(|failure| failure.node.as_ref());
        let last = state.finished.last().map(|finished| &finished.key);
        if let Some(key) = failed.or(last) {
            names.push(&key.node);
        }
    }
    names.join(", ")
}

/// The prompt the next try of `key` gets from `base` in `state`: when the last decision on its
/// gate was a revision, the comment and the previous result follow under "Revision requested:".
pub fn prompt_with_revision(state: &RunState, key: &NodeKey, base: &str) -> String {
    let last_gate = state
        .closed_gates
        .iter()
        .rev()
        .find(|gate| &gate.key == key);
    match last_gate.and_then(|gate| gate.comment.as_deref()) {
        Some(comment) => {
            let previous = state
                .finished
                .iter()
                .rev()
                .find(|finished| &finished.key == key && finished.succeeded())
                .and_then(|finished| finished.result.as_ref());
            revision_prompt(base, comment, previous)
        }
        None => base.to_string(),
    }
}

/// `base`, then the comment under "Revision requested:" and the previous result.
pub fn revision_prompt(base: &str, comment: &str, previous: Option<&Value>) -> String {
    let mut prompt = base.trim_end().to_string();
    prompt.push_str("\n\n");
    prompt.push_str(REVISION_HEADING);
    prompt.push('\n');
    prompt.push_str(comment.trim());
    prompt.push('\n');
    if let Some(previous) = previous {
        prompt.push_str("\nYour previous result:\n");
        prompt.push_str(
            &serde_json::to_string_pretty(previous).unwrap_or_else(|_| previous.to_string()),
        );
        prompt.push('\n');
    }
    prompt
}

/// `text` in single quotes for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn write_record(run_dir: &Path, record: &RunToml) -> std::io::Result<()> {
    fs::create_dir_all(run_dir)?;
    let path = run_dir.join(RUN_FILE);
    let partial = run_dir.join(format!("{RUN_FILE}.partial"));
    fs::write(&partial, record.to_toml())?;
    fs::rename(partial, path)
}

/// Copies the directory `from` to `to`, which must not exist yet; symbolic links are skipped.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    fs::create_dir_all(to)?;
    for item in fs::read_dir(from)? {
        let item = item?;
        let kind = item.file_type()?;
        let target = to.join(item.file_name());
        if kind.is_dir() {
            copy_dir(&item.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(item.path(), target)?;
        }
    }
    Ok(())
}

/// A stable hash of the files under `dir` (FNV-1a over their relative paths and contents, in
/// path order), as `fnv1a64:<hex>`.
fn hash_dir(dir: &Path) -> String {
    fn walk(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
        for item in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = item.path();
            if path.is_dir() {
                walk(root, &path, files);
            } else if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_path_buf());
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for file in files {
        feed(file.to_string_lossy().as_bytes());
        feed(&[0]);
        feed(&fs::read(dir.join(&file)).unwrap_or_default());
        feed(&[0]);
    }
    format!("fnv1a64:{hash:016x}")
}
