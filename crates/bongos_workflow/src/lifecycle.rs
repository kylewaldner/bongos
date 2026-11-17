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
//! task's `runs/` directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_work::ActiveRun;
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{TodoState, new_id, now};
use bongos_workflow_model::definition::{AgentKind, Node, NodeKind};
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::{
    Attempt, EndState, RUN_SCHEMA, RecordedInput, StartedBy, TodoRef,
};
use bongos_workflow_model::{
    Definition, InputValue, Layer, LayerDirs, NodeKey, RunState, RunToml, check_fresh_retry, parse,
    retry_set, schema,
};
use serde_json::Value;

use crate::WorkflowsSettings;
use crate::definitions::{self, DefinitionError};
use crate::mcp;
use crate::runtime::{RunBinding, RunSpec, Runner, Runtime, RuntimeError, Settlement};
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

/// What `start` is asked to start.
#[derive(Debug, Clone, PartialEq)]
pub struct StartRequest {
    pub task_id: String,
    /// The todo the run completes; `None` is the task's current todo (the first open one).
    pub todo_index: Option<usize>,
    /// The workflow to run; `None` is the one the todo names.
    pub workflow: Option<String>,
    /// Input values that win over the definition's sources.
    pub overrides: BTreeMap<String, InputValue>,
    pub started_by: StartedBy,
}

impl StartRequest {
    /// Kyle starts the todo's own workflow on the current todo of `task_id`.
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            todo_index: None,
            workflow: None,
            overrides: BTreeMap::new(),
            started_by: StartedBy::Person,
        }
    }
}

/// Why a command was refused, in the order start checks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    UnknownTask,
    Group,
    NoOpenTodo,
    ActiveRun,
    UnknownWorkflow,
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
        worktree: attempt_worktree(attempt),
        attempt: attempt.n,
        checkout: checkout.clone(),
        status,
        finished,
    }))
}

fn attempt_worktree(attempt: &Attempt) -> RunWorktree {
    let path = PathBuf::from(&attempt.worktree);
    RunWorktree {
        run_dir: path
            .parent()
            .map_or_else(|| path.clone(), Path::to_path_buf),
        path,
        branch: attempt.branch.clone(),
    }
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
    task: TaskCheckout,
    /// `<task dir>/runs/<id>`.
    dir: PathBuf,
    record: RunToml,
    worktree: RunWorktree,
    /// The definition revision in use within the current attempt.
    rev: u32,
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
        }
    }
}

/// `<run dir>/attempts/<n>`.
pub fn attempt_dir(run_dir: &Path, attempt: u32) -> PathBuf {
    run_dir.join("attempts").join(attempt.to_string())
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
        }
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
        let changed = self.runtime.tick();
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

    /// Starts a run on the task and todo of `request`. Refused, in this order: a group, no open
    /// todo, an active run, an unknown workflow, a definition with errors, a missing required
    /// input, an agent CLI that cannot run, a missing interpreter, an allowed MCP server not
    /// installed, too little free space, no task worktree. Nothing is written before every check
    /// passed but the last two, which the worktree creation makes.
    pub fn start(
        &mut self,
        request: StartRequest,
        dirs: &LayerDirs,
        context: &StartContext<'_>,
    ) -> Result<RunId, Refusal> {
        let tree = context.tree;
        let task = tree.task_by_id(&request.task_id).ok_or_else(|| {
            Refusal::new(
                RefusalKind::UnknownTask,
                format!("No task {} is loaded.", request.task_id),
                "Pick a task in the Work view.",
            )
        })?;
        if derive::shape(task, tree) == Shape::Group {
            return Err(Refusal::new(
                RefusalKind::Group,
                "Groups have no todos.",
                "Pick a leaf, or use `task: run`.",
            ));
        }
        let (todo_index, todo) = open_todo(task, request.todo_index).ok_or_else(|| {
            Refusal::new(
                RefusalKind::NoOpenTodo,
                "This task has no open todo for a workflow to complete.",
                "Add one (todo: add), or reopen one.",
            )
        })?;
        let reported = context.active_run.clone();
        if let Some(refusal) = self.active_refusal(&request.task_id, reported) {
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
        let (resolved, definition) = load_definition(dirs, &name, Some(context.checks))?;
        let inputs = bongos_workflow_model::resolve(
            &definition,
            task,
            tree,
            Some(todo_index),
            &request.overrides,
        );
        if let Some(missing) = inputs.iter().find(|input| input.missing) {
            return Err(Refusal::new(
                RefusalKind::MissingInput,
                format!(
                    "Workflow '{name}' needs the input '{}' (from {}), which has no value.",
                    missing.name, missing.source
                ),
                "Fill it in on the task, or give it when starting.",
            ));
        }
        check_machine(&definition, context.checks)?;

        let file = task.file.as_ref().ok_or_else(|| {
            Refusal::new(
                RefusalKind::NoTaskWorktree,
                "This task has no worktree.",
                "Start the task first (task: start).",
            )
        })?;
        let checkout = TaskCheckout::from_task(task.slug.clone(), file);
        let id = new_id();
        let worktree = self.create_worktree(&checkout, &id, 1)?;
        let dir = task.dir.join(RUNS_DIR).join(&id);
        let created = now().to_string();
        let record = RunToml {
            schema: RUN_SCHEMA,
            id: id.clone(),
            workflow: name,
            layer: resolved.layer.label().to_string(),
            task_id: request.task_id.clone(),
            created: created.clone(),
            started_by: request.started_by,
            todo: TodoRef {
                index: todo_index,
                name: todo.name.clone(),
            },
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
        let mut entry = RunEntry {
            id: id.clone(),
            task_id: request.task_id,
            task: checkout,
            dir,
            record,
            worktree,
            rev: 0,
        };
        let commit = context
            .definition_commit
            .clone()
            .filter(|_| resolved.layer == Layer::Project);
        self.begin_attempt(&mut entry, 1, &resolved.dir, definition, created, commit)?;
        self.runs.push(entry);
        self.note(&id);
        Ok(id)
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
        let mut report = LoadReport::default();
        let mut entries: Vec<RunEntry> = Vec::new();
        let mut specs: Vec<RunSpec> = Vec::new();
        let mut seen: std::collections::BTreeSet<RunId> = std::collections::BTreeSet::new();
        for task in tree.tasks() {
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
            for dir in dirs {
                match self.read_run(task, &dir) {
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
            self.runtime.bind(&entry.id, entry.binding());
            report.loaded.push(entry.id.clone());
            report.orphans.push((entry.id.clone(), orphans));
            self.runs.push(entry);
        }
        self.note_all();
        Ok(report)
    }

    /// One run directory: `Ok(None)` for a cleared run.
    fn read_run(&self, task: &Task, dir: &Path) -> Result<Option<(RunEntry, RunSpec)>, String> {
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
        let rev = bongos_workflow_model::record::replay(&parsed.events)
            .definition_rev
            .unwrap_or(0);
        let definition = read_definition(&definition_copy_dir(&attempt_dir, rev))?;
        let worktree = attempt_worktree(attempt);
        let checkout = match &task.file {
            Some(file) => TaskCheckout::from_task(task.slug.clone(), file),
            None => TaskCheckout {
                slug: task.slug.clone(),
                worktree: None,
                branch: None,
            },
        };
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
            task_id: record.task_id.clone(),
            task: checkout,
            dir: dir.to_path_buf(),
            record,
            worktree,
            rev,
        };
        Ok(Some((entry, spec)))
    }

    /// `workflow: clear finished`, the first half: every run that ended succeeded or was
    /// cancelled leaves the lifecycle. Its `cleared` marker is written first (and synced), then the
    /// run is dropped from the runtime; a run whose marker cannot be written stays listed and is
    /// reported in [`ClearOutcome::failed`]. A failed run counts as active and stays, and so does
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
    /// directory), for editing a halted run by hand.
    pub fn halt_worktrees(&self, run: &str) -> Option<HaltWorktrees> {
        let entry = self.entry(run).ok()?;
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
        self.runtime.resume(run).map_err(Refusal::runtime)?;
        self.note(run);
        Ok(())
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
        let status = self.status_of(run)?;
        if status == RunStatus::Succeeded {
            return Err(wrong_state(status, "restart"));
        }
        if !status.is_active() {
            let task = self.runs[index].task_id.clone();
            if self.active_run_id(&task).is_some() {
                if let Some(refusal) = self.active_refusal(&task, None) {
                    return Err(refusal);
                }
            }
        }
        let name = self.runs[index].record.workflow.clone();
        let (resolved, definition) = load_definition(dirs, &name, None)?;
        let attempt = self.runs[index].attempt() + 1;
        let checkout = self.runs[index].task.clone();
        let worktree = self.create_worktree(&checkout, run, attempt)?;
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
        self.open_gate(run, key)?;
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
        let directory = match finished.key.branch.last() {
            Some((fan, index)) => branch_worktree_path(&entry.worktree.run_dir, fan, *index),
            None => entry.worktree.path.clone(),
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
            .map_err(|error| match error {
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
            })
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
        let (resolved, _) = load_definition(dirs, &name, None)?;
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

/// Resolves and checks the definition `name`: refused when unknown or when it has errors. With
/// `checks` that know a CLI's MCP servers, validation leaves that CLI's servers to the start's own
/// MCP check, which comes later in the refusal order; otherwise validation reports them.
fn load_definition(
    dirs: &LayerDirs,
    name: &str,
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
    let base = checks.map(|checks| checks.validation()).unwrap_or_default();
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
