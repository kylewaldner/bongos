//! The engine runtime (feature 20, D12): the run loop over [`scheduler::step`], the run, session
//! and branch limits, the Queued state, halt, cancel, quit and the load after a relaunch.
//!
//! The runtime owns the live runs, each with its open `events.jsonl` (D5). [`Runtime::tick`] takes
//! the finished tries from the [`Runner`], appends them, then asks [`scheduler::step`] what each
//! executing run does next and carries it out, appending one event per command, until nothing
//! changes. The run state is never kept apart from the log: it is [`replay`] of the events.
//!
//! Limits ([`Limits`], from the `workflows` settings):
//! - Runs: at most `max_concurrent_runs` hand-started runs execute at once. The rest wait Queued
//!   in start order (a `Queued` event) and start when a slot frees. A lane's or a landing check's
//!   run is not counted and never waits for a slot ([`StartedBy::counts_toward_run_limit`]). A run
//!   held back ([`Runtime::hold_back`], a definition at its `instances`) waits Queued until it is
//!   let start. A run holds its slot while it has a process
//!   running or something to do now; gates, sleeping polls and Queued nodes hold no slot (D12), so
//!   a run whose only live work waits on those is parked and frees its slot. A parked run that has
//!   work again (a gate closed, a poll is due) waits for a slot behind the runs already queued,
//!   without a new `Queued` event. A run held for Kyle (ended, cancelled, halted, interrupted)
//!   holds none either.
//! - Sessions: the processes executing across all runs never exceed the session limit; the free
//!   count is the `free` of [`scheduler::step`]. Gates, sleeping polls and Queued runs hold none.
//! - Branches: the scheduler keeps a run's executing branches within `max_parallel_branches`.
//!
//! - Checks: a key of a `use = "checks"` program node holds no session slot (pools govern checks)
//!   and gets no time or output cap.
//!
//! Built-ins: a key of a program node whose `use` is registered in [`BuiltinNodes`] is started,
//! polled and stopped through that built-in instead of the [`Runner`]; it is recorded with pid 0.
//!
//! Halt and cancel stop the run's processes through [`Runner::stop`], within 5 s (D12). Quit stops
//! every process and appends `Interrupted` with the keys in flight. [`Runtime::load`] never resumes
//! a run by itself: it stops processes a crash left behind (only those whose pid and start time
//! both match the record), marks a run with a started key and no finish interrupted, lets each
//! built-in clean up after its interrupted keys ([`BuiltinNode::recover`]), and gives an
//! interrupted, halted, ended, cancelled or held run no command until Kyle acts.
//!
//! Drain and hand-over: [`Runtime::drain`] starts no new node in any run and resolves once no
//! node process runs (gates and sleeping polls hold none; a checks key whose check was stopped
//! counts as parked once [`Runtime::checks_drained`] recorded it). [`Runtime::hand_over`] then
//! appends `HandedOver` to every live run and [`Runtime::unload`] drops runs without writing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_work_model::store::Project;
use bongos_workflow_model::inputs::Input;
use bongos_workflow_model::record::{RunningNode, StartedBy};
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::unsandboxed::{self, Mode, Permit};
use bongos_workflow_model::{
    Command, Definition, EnvEdit, Event, Failure, FailureReason, LaneRef, Limits, NodeKey,
    RunState, record, replay, scheduler,
};
use futures::channel::oneshot;
use serde_json::Value;

use crate::WorkflowsSettings;
use crate::builtin_nodes::{BuiltinNode, BuiltinNodes, BuiltinStart};
use crate::consent::{self, Grant, RunConsentStore, RunConsents};
use crate::process::{OrphanOutcome, ProcessRecord, kill_orphans};
use crate::worktrees::{self, Git, RunWorktree, TaskCheckout, WorktreeError};

/// The name of the checkpoint log in an attempt directory (D5).
pub const EVENTS_FILE: &str = "events.jsonl";

/// Creates the run worktree of a run with the app's settings and the real free-space check
/// ([`worktrees::disk_free_bytes`]); when the free space cannot be read the start goes ahead with
/// a warning.
pub async fn create_run_worktree(
    git: &Git,
    task: &TaskCheckout,
    run_id: &str,
    attempt: u32,
    settings: &WorkflowsSettings,
) -> Result<RunWorktree, WorktreeError> {
    worktrees::create_run(
        git,
        task,
        run_id,
        attempt,
        settings,
        &worktrees::disk_free_bytes,
    )
    .await
}

/// A run handed to the runtime: its id, the definition it runs and its attempt directory, which
/// holds `events.jsonl`.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub id: String,
    pub definition: Definition,
    pub attempt_dir: PathBuf,
    /// The loop bound of the attempt (`Attempt::loop_bound`, or the setting for an old record):
    /// what the scheduler applies to a loop whose route sets no `max_passes`.
    pub loop_bound: u32,
}

/// A try that ended, as a [`Runner`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Finish {
    pub run: String,
    pub key: NodeKey,
    pub try_number: u32,
    /// The typed result, or the failure.
    pub outcome: Result<Value, Failure>,
    pub cost_usd: f64,
    pub turns: Option<u32>,
    pub elapsed_ms: u64,
    pub session_id: Option<String>,
}

impl Finish {
    /// A try that ended with `result` and no cost.
    pub fn ok(run: impl Into<String>, key: NodeKey, try_number: u32, result: Value) -> Self {
        Self {
            run: run.into(),
            key,
            try_number,
            outcome: Ok(result),
            cost_usd: 0.0,
            turns: None,
            elapsed_ms: 0,
            session_id: None,
        }
    }
}

/// Where a run lives and what it was started with: what a runner needs to start its nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct RunBinding {
    pub workflow: String,
    pub task_id: String,
    /// The task's directory.
    pub task_dir: PathBuf,
    /// `<task dir>/runs/<id>`.
    pub run_dir: PathBuf,
    /// The directory of the current attempt.
    pub attempt_dir: PathBuf,
    pub worktree: RunWorktree,
    pub checkout: TaskCheckout,
    /// The run's resolved inputs.
    pub inputs: Vec<Input>,
    /// What the run was started with besides its task; the default is a task run's.
    pub setup: RunSetup,
}

/// What a run was started with besides its task: its lane, its edits to the environment, whether
/// its tree is a read-only snapshot, and for a run with no task where its agents' context comes
/// from. The default is a task run's as it always was: no lane, no edit, a writable run worktree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunSetup {
    pub lane: Option<LaneRef>,
    /// The lane's named directories, by name.
    pub lane_dirs: BTreeMap<String, PathBuf>,
    /// The run's edits to its nodes' environment, applied in order.
    pub env: Vec<EnvEdit>,
    /// The run works on a snapshot: its worktree is read and never written, and it has no
    /// repository, run branch or fan-out branch worktrees.
    pub snapshot: bool,
    /// The run works in its project's work directory (`work/<run id>` of the project's files
    /// folder): writable, but with no repository, run branch or fan-out branch worktrees, and its
    /// programs run only inside the OS sandbox, which keeps the rest of the folder read-only.
    pub work_dir: bool,
    /// For a project subject: the project, its files folder and the definitions its agents are
    /// pointed at.
    pub project: Option<ProjectSource>,
    /// Who started the run and whether it runs unattended, which decide whether a program may
    /// ever run without the OS sandbox.
    pub facts: RunFacts,
}

/// What `run.toml` says about who is there to answer for a run: whether it runs unattended and
/// who started it. The default knows neither, so a program of it never runs unsandboxed outside
/// `fallback` (fail closed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunFacts {
    pub unattended: bool,
    /// `None` when the run's record was not read.
    pub started_by: Option<StartedBy>,
}

/// Where the agents of a project-subject run take their context from.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSource {
    pub project: Project,
    pub files_dir: PathBuf,
    pub definitions: PathBuf,
}

/// `BONGOS_LANE_DIR_<NAME>`: the name upper-cased, every character that is not an ASCII letter or
/// digit replaced with `_`.
pub fn lane_dir_variable(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("BONGOS_LANE_DIR_{name}")
}

impl RunSetup {
    /// The run has no branches: it works on a snapshot or in its project's work directory, so
    /// every key runs in its one tree and a join has nothing to merge.
    pub fn branchless(&self) -> bool {
        self.snapshot || self.work_dir
    }

    /// The edits every node of the run gets, in order: the run's own, less any that names one of
    /// bongos's own `BONGOS_*` variables, then for a lane run `BONGOS_LANE`, `BONGOS_LANE_KIND`
    /// and one `BONGOS_LANE_DIR_<NAME>` per named directory. So no edit changes a `BONGOS_*`
    /// variable and a run with no lane has none of the lane's. Empty for a run with no lane and
    /// no edit.
    pub fn env_edits(&self) -> Vec<EnvEdit> {
        let mut edits: Vec<EnvEdit> = self
            .env
            .iter()
            .filter(|edit| match edit {
                EnvEdit::Set { name, .. } | EnvEdit::Unset { name } => !name.starts_with("BONGOS_"),
                EnvEdit::PrependPath { .. } => true,
            })
            .cloned()
            .collect();
        if let Some(lane) = &self.lane {
            let set = |name: String, value: String| EnvEdit::Set { name, value };
            edits.push(set("BONGOS_LANE".to_string(), lane.id.clone()));
            edits.push(set("BONGOS_LANE_KIND".to_string(), lane.kind.clone()));
            for (name, dir) in &self.lane_dirs {
                edits.push(set(
                    lane_dir_variable(name),
                    dir.to_string_lossy().into_owned(),
                ));
            }
        }
        edits
    }

    /// The directories a program of the run may write besides its worktree and scratch: the
    /// lane's named directories, none for a run with no lane.
    pub fn writable_dirs(&self) -> Vec<PathBuf> {
        if self.lane.is_none() {
            return Vec::new();
        }
        self.lane_dirs.values().cloned().collect()
    }
}

/// One try a runner is asked to start, with the context of its run.
pub struct NodeStart<'a> {
    pub run: &'a str,
    pub definition: &'a Definition,
    pub key: &'a NodeKey,
    pub try_number: u32,
    pub state: &'a RunState,
    /// What is left of the run's cost cap; `None` when the run has none.
    pub budget_left_usd: Option<f64>,
    /// The run's binding, when the lifecycle bound it.
    pub binding: Option<&'a RunBinding>,
    /// A person's grant to run this run's programs without the OS sandbox, from the runtime's
    /// consent store; `None` when there is none.
    pub grant: Option<Grant>,
}

/// What runs node tries: the agent runners, the program runner and the built-ins, or a fake in
/// tests. The runtime decides; the runner only starts, reports and stops.
pub trait Runner {
    /// Starts try `try_number` of `key` of run `run`; the process it started, or why it could not.
    fn start(
        &mut self,
        run: &str,
        definition: &Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<ProcessRecord, Failure>;

    /// Records the context of `run`, when an attempt begins and when a run is loaded.
    fn bind(&mut self, _run: &str, _binding: RunBinding) {}

    /// Forgets the context of `run`, when the runtime drops it.
    fn unbind(&mut self, _run: &str) {}

    /// The effective `workflows.programs.unsandboxed` mode of `run`, which its built-ins start
    /// under; the default reads no setting.
    fn unsandboxed_mode(&self, _run: &str) -> Mode {
        Mode::default()
    }

    /// [`Runner::start`] with the run's state and binding; the runtime calls this one.
    fn start_node(&mut self, node: NodeStart<'_>) -> Result<ProcessRecord, Failure> {
        self.start(node.run, node.definition, node.key, node.try_number)
    }

    /// The tries that ended since the last call.
    fn poll(&mut self) -> Vec<Finish>;

    /// Stops the processes of these tries, all within 5 s ([`crate::process::stop_all`]). A
    /// stopped try is never reported by [`Runner::poll`].
    fn stop(&mut self, tries: &[(String, NodeKey)]);

    /// Merges a finished fan-out branch into the run branch before its join runs; the merge
    /// commit, or the failure (`merge conflict` naming the files).
    fn merge_branch(&mut self, run: &str, join: &str, branch: &Branch) -> Result<String, Failure>;
}

impl<R: Runner + ?Sized> Runner for Box<R> {
    fn start(
        &mut self,
        run: &str,
        definition: &Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        (**self).start(run, definition, key, try_number)
    }

    fn bind(&mut self, run: &str, binding: RunBinding) {
        (**self).bind(run, binding)
    }

    fn unbind(&mut self, run: &str) {
        (**self).unbind(run)
    }

    fn unsandboxed_mode(&self, run: &str) -> Mode {
        (**self).unsandboxed_mode(run)
    }

    fn start_node(&mut self, node: NodeStart<'_>) -> Result<ProcessRecord, Failure> {
        (**self).start_node(node)
    }

    fn poll(&mut self) -> Vec<Finish> {
        (**self).poll()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        (**self).stop(tries)
    }

    fn merge_branch(&mut self, run: &str, join: &str, branch: &Branch) -> Result<String, Failure> {
        (**self).merge_branch(run, join, branch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Waiting for a run slot.
    Queued,
    /// Holding a run slot: the scheduler is asked what it does next.
    Executing,
    /// Holding no slot: nothing runs and the scheduler had nothing to do (the run waits on gates
    /// or sleeping polls). Checked each tick; once it has work it is queued for a slot again.
    Parked,
    /// Waiting for Kyle or done: halted, interrupted, ended or cancelled. Gets no command.
    Held,
}

struct LiveRun {
    id: String,
    definition: Definition,
    log: File,
    events: Vec<Event>,
    status: Status,
    /// Tries this runtime started whose finish it has not seen.
    in_flight: BTreeSet<NodeKey>,
    /// The keys of `in_flight` a built-in runs, with that built-in.
    builtins: BTreeMap<NodeKey, Arc<dyn BuiltinNode>>,
    /// The run's context, once bound ([`Runtime::bind`]).
    binding: Option<RunBinding>,
    /// The attempt's loop bound; a later settings change reaches only new attempts.
    loop_bound: u32,
    /// Who started the run ([`Runtime::set_started_by`]); a person until told otherwise.
    started_by: StartedBy,
}

impl LiveRun {
    fn state(&self) -> RunState {
        replay(&self.events)
    }

    /// Appends `event` to the log (one line, then `sync_data`) and to the events in memory. A
    /// failed write is logged; the event is still applied, since what it records already
    /// happened.
    fn append(&mut self, event: Event) {
        if let Err(error) = record::append(&mut self.log, &event) {
            log::error!(
                "workflows: could not append to the log of run {}: {error}",
                self.id
            );
        }
        self.events.push(event);
    }

    /// Appends `event` only when the write succeeds: on a failed write the event is neither in
    /// memory nor taken as done, so the caller can refuse what it records.
    fn try_append(&mut self, event: Event) -> std::io::Result<()> {
        record::append(&mut self.log, &event)?;
        self.events.push(event);
        Ok(())
    }

    /// The number of the next try of `key`: one more than its `NodeStarted` events.
    fn next_try(&self, key: &NodeKey) -> u32 {
        self.events
            .iter()
            .filter(
                |event| matches!(event, Event::NodeStarted { key: started, .. } if started == key),
            )
            .count() as u32
            + 1
    }

    /// The tries in flight that hold a session slot: every one but a checks node's.
    fn sessions(&self) -> usize {
        self.in_flight
            .iter()
            .filter(|key| holds_session(&self.definition, key))
            .count()
    }

    /// Whether the run has started anything: a try or a gate.
    fn has_begun(&self) -> bool {
        self.events.iter().any(|event| {
            matches!(
                event,
                Event::NodeStarted { .. } | Event::GateOpened { .. } | Event::BranchMerged { .. }
            )
        })
    }

    /// Whether the run waits for Kyle or is done, so it gets no command and holds no run slot.
    fn is_held(state: &RunState) -> bool {
        state.cancelled
            || state.ended.is_some()
            || state.halted
            || !state.interrupted.is_empty()
            || !state.held.is_empty()
    }

    /// Whether the run is held only on its ask to run without the OS sandbox: every held key is
    /// held on the ask, and nothing else holds the run. The other starts of the same step still
    /// go ahead then, so keys that need the same answer join the one ask.
    fn held_on_ask_only(state: &RunState) -> bool {
        !state.cancelled
            && state.ended.is_none()
            && !state.halted
            && state.interrupted.is_empty()
            && state.consent_pending.is_some()
            && state
                .held
                .values()
                .all(|failure| failure.reason == FailureReason::SandboxRequired)
    }
}

/// Why a runtime command was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RuntimeError {
    #[error("no run {0} is loaded")]
    UnknownRun(String),
    #[error("run {0} is over")]
    Over(String),
    #[error("run {0} is already loaded")]
    Duplicate(String),
    #[error("{0}")]
    Io(String),
    #[error("workflows are draining: no node starts until the drain is cancelled")]
    Draining,
    #[error(
        "run {run} is over its cost cap: raise workflows.run_max_cost_usd above ${cost:.2} first"
    )]
    OverCap { run: String, cost: f64 },
    #[error("run {0} is not waiting for an answer about running without a sandbox")]
    NothingAsked(String),
}

impl RuntimeError {
    fn io(path: &Path, error: impl fmt::Display) -> Self {
        Self::Io(format!("{}: {error}", path.display()))
    }
}

/// How the engine settles a run the scheduler would end succeeded ([`Runtime::hold_success`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Settlement {
    /// End it succeeded.
    Succeed,
    /// End it failed with this failure.
    Fail(Failure),
    /// Open a gate on `key` (a key of no node, so the scheduler never walks it) and wait on Kyle;
    /// once it closes, the scheduler offers the end again and the engine settles it again.
    Wait { key: NodeKey, question: String },
}

/// The live runs and the loop that drives them.
pub struct Runtime<R: Runner> {
    limits: Limits,
    runner: R,
    /// In start order: that is the queue order and the order runs are stepped.
    runs: Vec<LiveRun>,
    /// Queued runs, oldest first.
    queue: VecDeque<String>,
    /// Set by [`Runtime::quit`]: nothing starts after it.
    quitting: bool,
    /// Whether a succeeded end waits for the engine ([`Runtime::hold_success`]).
    hold_success: bool,
    /// Runs whose succeeded end waits for [`Runtime::settle`], oldest first, not yet taken.
    settling: Vec<String>,
    /// The built-in program nodes, run instead of through `runner`.
    builtins: BuiltinNodes,
    /// Queued runs not admitted until let start ([`Runtime::hold_back`]).
    held_back: BTreeSet<String>,
    /// Set by [`Runtime::drain`], cleared by [`Runtime::undrain`]: no node starts.
    draining: bool,
    /// The drains waiting for no node process to run.
    drain_waiters: Vec<oneshot::Sender<()>>,
    /// The grants to run without the OS sandbox, per run, in memory only.
    consents: Arc<dyn RunConsentStore>,
}

/// The most rounds one [`Runtime::tick`] runs; a round that changes nothing ends it sooner.
const MAX_ROUNDS: usize = 10_000;

impl<R: Runner> Runtime<R> {
    pub fn new(limits: Limits, runner: R) -> Self {
        Self {
            limits,
            runner,
            runs: Vec::new(),
            queue: VecDeque::new(),
            quitting: false,
            hold_success: false,
            settling: Vec::new(),
            builtins: BuiltinNodes::default(),
            held_back: BTreeSet::new(),
            draining: false,
            drain_waiters: Vec::new(),
            consents: RunConsents::store(),
        }
    }

    /// The store of the grants to run without the OS sandbox.
    pub fn consents(&self) -> &Arc<dyn RunConsentStore> {
        &self.consents
    }

    /// Keeps the grants in `store` from now on; the grants of the old store are not carried over.
    pub fn set_consents(&mut self, store: Arc<dyn RunConsentStore>) {
        self.consents = store;
    }

    /// Answers the pending ask of run `id` to run without the OS sandbox: appends
    /// `ConsentAnswered`, and only once it is written grants the run in the store when allowed.
    /// Replay of the answer releases the held keys (allowed: ready to start, here with the grant;
    /// stopped: failed `SandboxRefused`), so the run is queued again. Refused with nothing
    /// pending; a failed write refuses the answer and leaves the ask pending, with no grant.
    pub fn answer_consent(
        &mut self,
        id: &str,
        allowed: bool,
        by: &str,
        at: String,
    ) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        if self.runs[index].state().consent_pending.is_none() {
            return Err(RuntimeError::NothingAsked(id.to_string()));
        }
        let event = Event::ConsentAnswered {
            allowed,
            by: by.to_string(),
            at: at.clone(),
        };
        if let Err(error) = self.runs[index].try_append(event) {
            return Err(RuntimeError::Io(format!(
                "could not record the answer for run {id}: {error}"
            )));
        }
        if allowed {
            self.consents.grant(
                id,
                Grant {
                    at,
                    by: by.to_string(),
                },
            );
        }
        self.requeue(index);
        Ok(())
    }

    /// Records who started run `id`: a lane's or a landing check's run does not count toward
    /// `max_concurrent_runs` ([`StartedBy::counts_toward_run_limit`]).
    pub fn set_started_by(&mut self, id: &str, started_by: StartedBy) {
        if let Some(index) = self.find(id) {
            self.runs[index].started_by = started_by;
        }
    }

    /// Keeps queued run `id` from being admitted (it stays Queued) until [`Runtime::let_start`].
    pub fn hold_back(&mut self, id: &str) {
        if self.find(id).is_some() {
            self.held_back.insert(id.to_string());
        }
    }

    /// Lets a run held back by [`Runtime::hold_back`] be admitted at the next tick.
    pub fn let_start(&mut self, id: &str) {
        self.held_back.remove(id);
    }

    /// Whether run `id` is held back.
    pub fn is_held_back(&self, id: &str) -> bool {
        self.held_back.contains(id)
    }

    /// Whether run `id` has started anything: a try, a gate or a merge.
    pub fn has_begun(&self, id: &str) -> bool {
        self.find(id)
            .is_some_and(|index| self.runs[index].has_begun())
    }

    /// Starts no new node in any run from now on (the running ones finish and are recorded),
    /// until [`Runtime::undrain`]. The receiver resolves once no node process runs: gates and
    /// sleeping polls hold none, and a checks key counts as parked once
    /// [`Runtime::checks_drained`] recorded it. An undrain before then drops it unresolved
    /// (`Canceled`).
    pub fn drain(&mut self) -> oneshot::Receiver<()> {
        self.draining = true;
        let (sender, receiver) = oneshot::channel();
        self.drain_waiters.push(sender);
        self.notify_drained();
        receiver
    }

    /// Cancels a drain: nodes start again at the next tick, and the drains still waiting are
    /// dropped unresolved.
    pub fn undrain(&mut self) {
        self.draining = false;
        self.drain_waiters.clear();
    }

    /// Whether a drain is in force.
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// The node tries in flight across all runs, checks keys included.
    pub fn nodes_in_flight(&self) -> usize {
        self.runs.iter().map(|run| run.in_flight.len()).sum()
    }

    /// Stops the try of checks key `key` of run `id` for a drain and appends `ChecksDrained`:
    /// the key no longer runs, and the run starts it again from its checkpoint once nodes start
    /// again (here after an undrain, or in the process the run is handed over to).
    pub fn checks_drained(&mut self, id: &str, key: &NodeKey) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        let run = &mut self.runs[index];
        let is_checks = run
            .definition
            .node(&key.node)
            .is_some_and(bongos_workflow_model::definition::Node::is_checks);
        if !is_checks || !run.in_flight.remove(key) {
            return Err(RuntimeError::Io(format!(
                "run {id} has no check of {key} in flight"
            )));
        }
        self.stop_tries(vec![(id.to_string(), key.clone())]);
        self.runs[index].append(Event::ChecksDrained { key: key.clone() });
        self.notify_drained();
        Ok(())
    }

    /// Appends `HandedOver { to_pid }` to every loaded run that is not over, once each.
    pub fn hand_over(&mut self, to_pid: u32) {
        for run in &mut self.runs {
            if !run.state().is_over() {
                run.append(Event::HandedOver { to_pid });
            }
        }
    }

    /// [`Self::hand_over`] for the runs `ids` only: `HandedOver { to_pid }` once to each of
    /// them that is loaded and not over.
    pub fn hand_over_runs(&mut self, to_pid: u32, ids: &[String]) {
        for run in &mut self.runs {
            if ids.contains(&run.id) && !run.state().is_over() {
                run.append(Event::HandedOver { to_pid });
            }
        }
    }

    /// Drops run `id` from the runtime without appending any event or stopping anything (a run
    /// handed over belongs to the other process now). Whether it was loaded.
    pub fn unload(&mut self, id: &str) -> bool {
        let Some(index) = self.find(id) else {
            return false;
        };
        self.forget(index);
        self.notify_drained();
        true
    }

    /// Resolves every drain waiting once nothing is in flight.
    fn notify_drained(&mut self) {
        if self.draining && self.nodes_in_flight() == 0 {
            for waiter in self.drain_waiters.drain(..) {
                waiter.send(()).ok();
            }
        }
    }

    /// What [`scheduler::step`] gives run `index` now, less every start while draining.
    fn commands(&self, index: usize, state: &RunState) -> Vec<Command> {
        let free = self.limits.free_sessions(self.sessions_in_flight());
        let mut commands = scheduler::step(
            &self.runs[index].definition,
            state,
            &self.limits_for(index),
            free,
        );
        if self.draining {
            commands.retain(|command| !matches!(command, Command::Start(_)));
        }
        commands
    }

    /// Runs the program nodes whose `use` is registered in `builtins` through them.
    pub fn set_builtins(&mut self, builtins: BuiltinNodes) {
        self.builtins = builtins;
    }

    pub fn builtins(&self) -> &BuiltinNodes {
        &self.builtins
    }

    /// With `hold` on, a run the scheduler ends succeeded is not ended: it is held for the engine,
    /// which takes it with [`Runtime::take_settling`] (to deliver its outcomes, offer the final
    /// merge and complete its todo) and ends it with [`Runtime::settle`]. Off by default, when the
    /// scheduler's end is appended as it comes.
    pub fn hold_success(&mut self, hold: bool) {
        self.hold_success = hold;
    }

    /// The runs held at a succeeded end since the last call, oldest first. Each is taken once:
    /// it is offered again only after a [`Settlement::Wait`] gate closes.
    pub fn take_settling(&mut self) -> Vec<String> {
        std::mem::take(&mut self.settling)
    }

    /// Ends or holds a run taken with [`Runtime::take_settling`]. Refused when the run is over
    /// (cancelled meanwhile, say) or the scheduler would no longer end it succeeded.
    pub fn settle(&mut self, id: &str, settlement: Settlement) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        let state = self.runs[index].state();
        let free = self.limits.free_sessions(self.sessions_in_flight());
        let ends_succeeded = scheduler::step(
            &self.runs[index].definition,
            &state,
            &self.limits_for(index),
            free,
        )
        .iter()
        .any(|command| {
            matches!(
                command,
                Command::End {
                    state: record::EndState::Succeeded,
                    ..
                }
            )
        });
        if !ends_succeeded || !state.open_gates.is_empty() || !self.runs[index].in_flight.is_empty()
        {
            return Err(RuntimeError::Io(format!(
                "run {id} is not waiting to end succeeded"
            )));
        }
        let run = &mut self.runs[index];
        match settlement {
            Settlement::Succeed => run.append(Event::Ended {
                state: record::EndState::Succeeded,
                failure: None,
            }),
            Settlement::Fail(failure) => run.append(Event::Ended {
                state: record::EndState::Failed,
                failure: Some(failure),
            }),
            Settlement::Wait { key, question } => run.append(Event::GateOpened { key, question }),
        }
        self.hold(index);
        Ok(())
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }

    pub fn runner_mut(&mut self) -> &mut R {
        &mut self.runner
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// The limits for run `index`: the current ones with the loop bound of the run's attempt.
    fn limits_for(&self, index: usize) -> Limits {
        Limits {
            loop_bound: self.runs[index].loop_bound,
            ..self.limits
        }
    }

    /// The limits in force for run `id`: the current ones with its attempt's loop bound.
    pub fn run_limits(&self, id: &str) -> Limits {
        self.find(id)
            .map_or(self.limits, |index| self.limits_for(index))
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The ids of the loaded runs, in start order.
    pub fn run_ids(&self) -> Vec<String> {
        self.runs.iter().map(|run| run.id.clone()).collect()
    }

    /// The replayed state of run `id`.
    pub fn state(&self, id: &str) -> Option<RunState> {
        self.find(id).map(|index| self.runs[index].state())
    }

    /// The events of run `id`, as in its log.
    pub fn events(&self, id: &str) -> Option<&[Event]> {
        self.find(id)
            .map(|index| self.runs[index].events.as_slice())
    }

    /// The place of run `id` among the runs waiting for a run slot (1 is next), `None` when it
    /// does not wait for one.
    pub fn queue_position(&self, id: &str) -> Option<usize> {
        self.queue
            .iter()
            .position(|queued| queued == id)
            .map(|index| index + 1)
    }

    /// Whether run `id` is waiting for a run slot.
    pub fn is_queued(&self, id: &str) -> bool {
        self.queue.iter().any(|queued| queued == id)
    }

    /// The processes executing across all runs that hold a session slot (a checks node's key
    /// holds none).
    pub fn sessions_in_flight(&self) -> usize {
        self.runs.iter().map(LiveRun::sessions).sum()
    }

    /// Starts a new run: creates its log with `Created` and queues it for a run slot; the next
    /// [`Runtime::tick`] starts it, or appends `Queued` while every slot is taken.
    pub fn add(&mut self, spec: RunSpec) -> Result<(), RuntimeError> {
        if self.find(&spec.id).is_some() {
            return Err(RuntimeError::Duplicate(spec.id));
        }
        let mut run = open_run(spec)?;
        if run.events.is_empty() {
            run.append(Event::Created);
        }
        self.queue.push_back(run.id.clone());
        run.status = Status::Queued;
        self.runs.push(run);
        Ok(())
    }

    /// Keeps `binding` as the context of run `id`, to hand to its node starts, and gives it to
    /// the runner.
    pub fn bind(&mut self, id: &str, binding: RunBinding) {
        if let Some(index) = self.find(id) {
            self.runs[index].binding = Some(binding.clone());
        }
        self.runner.bind(id, binding);
    }

    /// Loads a run after a relaunch; [`Runtime::load_all`] with one run.
    pub fn load(&mut self, spec: RunSpec) -> Result<Vec<(NodeKey, OrphanOutcome)>, RuntimeError> {
        self.load_all(vec![spec])?
            .pop()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    /// Loads runs after a relaunch. First every process the logs record as running (or
    /// interrupted with a process) is stopped when its pid is alive with the recorded start time,
    /// all runs' processes together in one shared grace period; a pid with another start time is
    /// a different process and is left alone. Then a run with a started key and no finish is
    /// marked interrupted with every such key. An interrupted, halted, ended or cancelled run gets
    /// no command until Kyle acts; any other run (queued, at a gate, with a sleeping poll) waits
    /// for a run slot like a new one. Nothing starts here. A duplicate id loads none of them. A log
    /// that cannot be opened is logged and skips only that run. Returns one entry per spec, in
    /// `specs` order: the run's orphan outcomes, or the error that kept it from loading.
    pub fn load_all(
        &mut self,
        specs: Vec<RunSpec>,
    ) -> Result<Vec<Result<Vec<(NodeKey, OrphanOutcome)>, RuntimeError>>, RuntimeError> {
        let mut runs: Vec<LiveRun> = Vec::with_capacity(specs.len());
        // Per spec: the failed open's error, or `None` for a run that opened (in `runs` order).
        let mut failures: Vec<Option<RuntimeError>> = Vec::with_capacity(specs.len());
        for spec in specs {
            if self.find(&spec.id).is_some() || runs.iter().any(|run| run.id == spec.id) {
                return Err(RuntimeError::Duplicate(spec.id));
            }
            let id = spec.id.clone();
            match open_run(spec) {
                Ok(run) => {
                    runs.push(run);
                    failures.push(None);
                }
                Err(error) => {
                    log::error!("workflows: run {id} not loaded: {error}");
                    failures.push(Some(error));
                }
            }
        }
        let mut keys: Vec<Vec<NodeKey>> = Vec::with_capacity(runs.len());
        let mut records: Vec<ProcessRecord> = Vec::new();
        for run in &runs {
            let state = run.state();
            let (run_keys, run_records): (Vec<NodeKey>, Vec<ProcessRecord>) =
                state
                    .running
                    .iter()
                    .chain(state.interrupted.iter().filter_map(|(key, running)| {
                        running.as_ref().map(|running| (key, running))
                    }))
                    .map(|(key, running)| (key.clone(), process_record(running)))
                    .unzip();
            keys.push(run_keys);
            records.extend(run_records);
        }
        let mut outcomes = kill_orphans(&records).into_iter();
        let mut orphans = Vec::with_capacity(runs.len());
        for (mut run, run_keys) in runs.into_iter().zip(keys) {
            orphans.push(
                run_keys
                    .into_iter()
                    .zip(outcomes.by_ref())
                    .collect::<Vec<_>>(),
            );
            let state = run.state();
            if !state.running.is_empty() && !state.halted && !state.is_over() {
                let keys: Vec<NodeKey> = state.running.keys().cloned().collect();
                run.append(Event::Interrupted { keys });
            }
            let state = run.state();
            // Replay never grants: a run loaded from disk starts with no grant, so a key its
            // log released from an ask asks again.
            self.consents.forget(&run.id);
            self.recover_builtins(&run, &state);
            if LiveRun::is_held(&state) {
                run.status = Status::Held;
            } else {
                run.status = Status::Queued;
                self.queue.push_back(run.id.clone());
            }
            self.runs.push(run);
        }
        let mut orphans = orphans.into_iter();
        Ok(failures
            .into_iter()
            .map(|failure| match failure {
                Some(error) => Err(error),
                None => Ok(orphans.next().unwrap_or_default()),
            })
            .collect())
    }

    /// One pass of the run loop: appends the tries that ended, admits queued runs while run slots
    /// are free, and carries out what [`scheduler::step`] gives each executing run, until nothing
    /// changes. Whether anything changed.
    pub fn tick(&mut self) -> bool {
        let changed = self.run_rounds();
        self.notify_drained();
        changed
    }

    fn run_rounds(&mut self) -> bool {
        if self.quitting {
            return false;
        }
        let mut changed = self.take_finishes();
        changed |= self.check_caps();
        for _ in 0..MAX_ROUNDS {
            let woken = self.wake_parked();
            let admitted = self.admit_queued();
            let stepped = self.step_runs();
            if !woken && !admitted && !stepped {
                return changed;
            }
            changed = true;
        }
        log::error!("workflows: the run loop did not settle in {MAX_ROUNDS} rounds");
        changed
    }

    /// Halts run `id`: stops its processes within 5 s and appends `Halted`. The stopped keys stay
    /// recorded as running, so a resume reruns them ([`scheduler::resume_set`]).
    pub fn halt(&mut self, id: &str) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        self.stop_in_flight(&[index]);
        let run = &mut self.runs[index];
        if !run.state().halted {
            run.append(Event::Halted);
        }
        // A run held back at its workflow's instance limit stays held back while halted, so a
        // resume does not start it past the limit.
        let held_back = self.held_back.contains(id);
        self.hold(index);
        if held_back {
            self.held_back.insert(id.to_string());
        }
        self.notify_drained();
        Ok(())
    }

    /// Cancels run `id` (also closing out a Failed run): stops its processes within 5 s and
    /// appends `Cancelled`.
    pub fn cancel(&mut self, id: &str) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        self.stop_in_flight(&[index]);
        self.runs[index].append(Event::Cancelled);
        self.hold(index);
        self.notify_drained();
        Ok(())
    }

    /// Pauses run `id`: appends `Paused`. No new key starts; the running ones finish, and the
    /// run ends once nothing is left to do (the scheduler's paused rule).
    pub fn pause(&mut self, id: &str) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        if !self.runs[index].state().paused {
            self.runs[index].append(Event::Paused);
        }
        Ok(())
    }

    /// Resumes run `id`: appends `Resumed` when it was paused, halted or held, then starts a new
    /// try of every key a halt stopped, a quit interrupted or a hold named
    /// ([`scheduler::resume_set`] less the keys still in flight), in the same worktree. A resumed
    /// try does not count against `retries`, and completed keys are not rerun. The run then
    /// waits for a run slot. The keys started.
    pub fn resume(&mut self, id: &str) -> Result<Vec<NodeKey>, RuntimeError> {
        let index = self.live(id)?;
        if self.draining {
            return Err(RuntimeError::Draining);
        }
        let state = self.runs[index].state();
        // The cap is whatever the limits say now (`set_limits` follows the settings), never a
        // value kept from when the run halted.
        if let Some((_, cost)) = cap_crossing(&state, self.limits.run_max_cost_usd) {
            return Err(RuntimeError::OverCap {
                run: id.to_string(),
                cost,
            });
        }
        if state.paused || state.halted || !state.held.is_empty() {
            self.runs[index].append(Event::Resumed);
        }
        let keys: Vec<NodeKey> = scheduler::resume_set(&state)
            .into_iter()
            .filter(|key| !self.runs[index].in_flight.contains(key))
            .collect();
        for key in &keys {
            self.start(index, key.clone());
        }
        self.requeue(index);
        Ok(keys)
    }

    /// Starts a new try of `key` now (a retry step) and queues the run for a slot. On a failed
    /// run the start reopens it and gives the key its retries again
    /// ([`RunState::retry_floors`]).
    pub fn rerun(&mut self, id: &str, key: NodeKey) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        if self.draining {
            return Err(RuntimeError::Draining);
        }
        if let Some((_, cost)) =
            cap_crossing(&self.runs[index].state(), self.limits.run_max_cost_usd)
        {
            return Err(RuntimeError::OverCap {
                run: id.to_string(),
                cost,
            });
        }
        self.start(index, key);
        self.requeue(index);
        Ok(())
    }

    /// Continues run `id` on `definition`, revision `rev` of the run (`DefinitionReloaded`).
    pub fn reload_definition(
        &mut self,
        id: &str,
        definition: Definition,
        rev: u32,
    ) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        let run = &mut self.runs[index];
        run.definition = definition;
        run.append(Event::DefinitionReloaded { rev });
        Ok(())
    }

    /// Closes the open gate of `key` (`GateClosed`); the run continues at its next tick.
    pub fn close_gate(
        &mut self,
        id: &str,
        key: NodeKey,
        by: &str,
        edit: Option<Value>,
        comment: Option<String>,
    ) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        self.runs[index].append(Event::GateClosed {
            key,
            by: by.to_string(),
            edit,
            comment,
        });
        self.requeue(index);
        Ok(())
    }

    /// Records a merge the engine made for run `id` itself, outside any join (the final merge
    /// of a definition with no merge node): `BranchMerged` with `join` and no branch. The
    /// scheduler matches merges to its joins by name, so it never counts this one.
    pub fn record_merge(&mut self, id: &str, join: &str, sha: String) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        self.runs[index].append(Event::BranchMerged {
            join: join.to_string(),
            branch: Vec::new(),
            sha,
        });
        Ok(())
    }

    /// Drops run `id` from the runtime, stopping its tries in flight first. Its log stays on
    /// disk. Whether it was loaded.
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(index) = self.find(id) else {
            return false;
        };
        self.stop_in_flight(&[index]);
        self.forget(index);
        self.notify_drained();
        true
    }

    /// Drops run `index` from every list, and its binding from the runner.
    fn forget(&mut self, index: usize) {
        let run = self.runs.remove(index);
        self.queue.retain(|queued| *queued != run.id);
        self.settling.retain(|settling| *settling != run.id);
        self.held_back.remove(&run.id);
        self.runner.unbind(&run.id);
        // A run dropped here (a restart, a handover) takes its grant with it.
        self.consents.forget(&run.id);
    }

    /// The definition run `id` runs now.
    pub fn definition(&self, id: &str) -> Option<&Definition> {
        self.find(id).map(|index| &self.runs[index].definition)
    }

    /// The keys of run `id` with a try in flight in this runtime.
    pub fn in_flight(&self, id: &str) -> Vec<NodeKey> {
        self.find(id)
            .map(|index| self.runs[index].in_flight.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Queues a held or parked run for a run slot again, after Kyle acted on it.
    fn requeue(&mut self, index: usize) {
        let run = &mut self.runs[index];
        if matches!(run.status, Status::Held | Status::Parked) {
            run.status = Status::Queued;
            let id = run.id.clone();
            if !self.queue.contains(&id) {
                self.queue.push_back(id);
            }
        }
    }

    /// Quit: stops every process of every run together (within 5 s) and appends `Interrupted`
    /// with every key in flight to each run that had one. Nothing starts afterwards.
    pub fn quit(&mut self) {
        self.quitting = true;
        self.interrupt_all();
        self.queue.clear();
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.runs.iter().position(|run| run.id == id)
    }

    fn live(&self, id: &str) -> Result<usize, RuntimeError> {
        let index = self
            .find(id)
            .ok_or_else(|| RuntimeError::UnknownRun(id.to_string()))?;
        if self.runs[index].state().is_over() {
            return Err(RuntimeError::Over(id.to_string()));
        }
        Ok(index)
    }

    fn hold(&mut self, index: usize) {
        let run = &mut self.runs[index];
        run.status = Status::Held;
        let id = run.id.clone();
        // An ended or cancelled run's grant ends with it: a retry step asks again.
        let state = run.state();
        if state.cancelled || state.ended.is_some() {
            self.consents.forget(&id);
        }
        self.queue.retain(|queued| *queued != id);
        self.held_back.remove(&id);
    }

    /// Stops the tries in flight of the runs at `indices` with one [`Runner::stop`], so they share
    /// one grace period.
    fn stop_in_flight(&mut self, indices: &[usize]) {
        let mut tries = Vec::new();
        for index in indices {
            let run = &mut self.runs[*index];
            for key in std::mem::take(&mut run.in_flight) {
                tries.push((run.id.clone(), key));
            }
        }
        self.stop_tries(tries);
    }

    /// Stops `tries` (already taken out of their runs' `in_flight`): each built-in's through it,
    /// the rest with one [`Runner::stop`].
    fn stop_tries(&mut self, tries: Vec<(String, NodeKey)>) {
        let mut by_runner = Vec::new();
        let mut by_builtin: Vec<(Arc<dyn BuiltinNode>, Vec<(String, NodeKey)>)> = Vec::new();
        for (id, key) in tries {
            let builtin = self
                .find(&id)
                .and_then(|index| self.runs[index].builtins.remove(&key));
            match builtin {
                Some(builtin) => {
                    match by_builtin
                        .iter_mut()
                        .find(|(node, _)| Arc::ptr_eq(node, &builtin))
                    {
                        Some((_, tries)) => tries.push((id, key)),
                        None => by_builtin.push((builtin, vec![(id, key)])),
                    }
                }
                None => by_runner.push((id, key)),
            }
        }
        for (builtin, tries) in by_builtin {
            builtin.stop(&tries);
        }
        if !by_runner.is_empty() {
            self.runner.stop(&by_runner);
        }
    }

    /// Tells each built-in which of its keys of `run` a crash or a quit left interrupted.
    fn recover_builtins(&self, run: &LiveRun, state: &RunState) {
        let mut by_builtin: Vec<(Arc<dyn BuiltinNode>, Vec<NodeKey>)> = Vec::new();
        for key in state.interrupted.keys() {
            let Some(builtin) = self.builtins.for_key(&run.definition, key) else {
                continue;
            };
            match by_builtin
                .iter_mut()
                .find(|(node, _)| Arc::ptr_eq(node, &builtin))
            {
                Some((_, keys)) => keys.push(key.clone()),
                None => by_builtin.push((builtin, vec![key.clone()])),
            }
        }
        for (builtin, keys) in by_builtin {
            builtin.recover(&run.id, &keys);
        }
    }

    fn take_finishes(&mut self) -> bool {
        let mut changed = false;
        let mut finishes = self.runner.poll();
        for builtin in self.builtins.all() {
            finishes.extend(builtin.poll());
        }
        for finish in finishes {
            let Some(index) = self.find(&finish.run) else {
                log::warn!("workflows: a try of unknown run {} finished", finish.run);
                continue;
            };
            let run = &mut self.runs[index];
            if !run.in_flight.remove(&finish.key) {
                // Stopped by a halt, a cancel or a join `any`: what it reports does not count.
                continue;
            }
            run.builtins.remove(&finish.key);
            let (result, failure) = match finish.outcome {
                Ok(value) => (Some(value), None),
                Err(failure) => (None, Some(failure)),
            };
            run.append(Event::NodeFinished {
                key: finish.key,
                try_number: finish.try_number,
                result,
                failure,
                cost_usd: finish.cost_usd,
                turns: finish.turns,
                elapsed_ms: finish.elapsed_ms,
                session_id: finish.session_id,
            });
            changed = true;
        }
        changed
    }

    /// Halts each run whose finished tries cost at or above the cap: stops its tries, appends
    /// `CapReached` (naming the key whose finish crossed it) and then `Halted`. Runs after every
    /// finish of the tick is appended, so a run halts once however many tries ended together.
    fn check_caps(&mut self) -> bool {
        let Some(cap) = self.limits.run_max_cost_usd else {
            return false;
        };
        let mut changed = false;
        for index in 0..self.runs.len() {
            let state = self.runs[index].state();
            if state.halted || state.is_over() || state.ended.is_some() {
                continue;
            }
            let Some((key, cost)) = cap_crossing(&state, Some(cap)) else {
                continue;
            };
            self.stop_in_flight(&[index]);
            let run = &mut self.runs[index];
            run.append(Event::CapReached {
                cap_usd: cap,
                cost_usd: cost,
                key,
            });
            run.append(Event::Halted);
            self.hold(index);
            changed = true;
        }
        changed
    }

    /// Stops every process of every run together (within 5 s) and appends `Interrupted` with the
    /// keys in flight to each run that had one, which then waits for a resume. Unlike
    /// [`Runtime::quit`] later starts still work (workflows were turned off, not the app). Whether
    /// any run was interrupted.
    pub fn interrupt_all(&mut self) -> bool {
        let indices: Vec<usize> = (0..self.runs.len()).collect();
        self.interrupt_indices(indices)
    }

    /// [`Self::interrupt_all`] for the runs `ids` only: their processes stop and each with a
    /// key in flight gets `Interrupted` and is held. The others keep running.
    pub fn interrupt_runs(&mut self, ids: &[String]) -> bool {
        let indices: Vec<usize> = ids.iter().filter_map(|id| self.find(id)).collect();
        self.interrupt_indices(indices)
    }

    fn interrupt_indices(&mut self, indices: Vec<usize>) -> bool {
        self.stop_in_flight(&indices);
        let mut changed = false;
        for index in indices {
            let state = self.runs[index].state();
            if state.running.is_empty() || state.halted || state.is_over() {
                continue;
            }
            let keys: Vec<NodeKey> = state.running.keys().cloned().collect();
            self.runs[index].append(Event::Interrupted { keys });
            self.hold(index);
            changed = true;
        }
        self.notify_drained();
        changed
    }

    /// Moves queued runs to executing, oldest first: a hand-started run while run slots are free,
    /// a lane's or a landing check's run always; a run held back waits. Appends `Queued` to each
    /// run left waiting that does not have it yet.
    fn admit_queued(&mut self) -> bool {
        let mut changed = false;
        let mut executing = self
            .runs
            .iter()
            .filter(|run| {
                run.status == Status::Executing && run.started_by.counts_toward_run_limit()
            })
            .count();
        let mut position = 0;
        while position < self.queue.len() {
            let id = &self.queue[position];
            let Some(index) = self.find(id) else {
                self.queue.remove(position);
                continue;
            };
            if self.held_back.contains(id) {
                position += 1;
                continue;
            }
            let counted = self.runs[index].started_by.counts_toward_run_limit();
            if counted && !self.limits.admits_run(executing) {
                position += 1;
                continue;
            }
            self.queue.remove(position);
            self.runs[index].status = Status::Executing;
            if counted {
                executing += 1;
            }
            changed = true;
        }
        for id in &self.queue {
            // A parked run queued again has already started; `Queued` marks only a run that
            // waits to begin.
            if let Some(run) = self.runs.iter_mut().find(|run| &run.id == id)
                && !run.state().queued
                && !run.has_begun()
            {
                run.append(Event::Queued);
                changed = true;
            }
        }
        changed
    }

    /// Queues each parked run that has work again (the scheduler gives it a command), so it waits
    /// for a run slot behind the runs already queued; drops a parked run that became held.
    fn wake_parked(&mut self) -> bool {
        let mut changed = false;
        for index in 0..self.runs.len() {
            if self.runs[index].status != Status::Parked {
                continue;
            }
            let state = self.runs[index].state();
            if LiveRun::is_held(&state) {
                self.hold(index);
                changed = true;
                continue;
            }
            if !self.commands(index, &state).is_empty() {
                let run = &mut self.runs[index];
                run.status = Status::Queued;
                self.queue.push_back(run.id.clone());
                changed = true;
            }
        }
        changed
    }

    /// Carries out one [`scheduler::step`] for each executing run, in start order. A run with
    /// nothing running and nothing to do is parked and frees its slot.
    fn step_runs(&mut self) -> bool {
        let mut changed = false;
        for index in 0..self.runs.len() {
            if self.runs[index].status != Status::Executing {
                continue;
            }
            let state = self.runs[index].state();
            if LiveRun::is_held(&state) {
                self.hold(index);
                changed = true;
                continue;
            }
            let commands = self.commands(index, &state);
            if commands.is_empty() {
                if state.running.is_empty() && self.runs[index].in_flight.is_empty() {
                    self.runs[index].status = Status::Parked;
                    changed = true;
                }
                continue;
            }
            let mut held = false;
            for command in commands {
                if held {
                    // Held only on the sandbox ask, the step's other starts still go ahead, so
                    // keys that need the same answer join the one ask; anything else waits.
                    let state = self.runs[index].state();
                    if !(LiveRun::held_on_ask_only(&state) && matches!(command, Command::Start(_)))
                    {
                        break;
                    }
                }
                changed = true;
                if !self.apply(index, command) {
                    // A merge failed: the rest of this step (more merges, starts) was planned
                    // before it, so it is dropped; the next round asks the scheduler again.
                    break;
                }
                held = LiveRun::is_held(&self.runs[index].state());
            }
            if LiveRun::is_held(&self.runs[index].state()) {
                self.hold(index);
            }
        }
        changed
    }

    /// Carries out one command; whether the rest of the step may still be carried out.
    fn apply(&mut self, index: usize, command: Command) -> bool {
        match command {
            Command::Start(key) => self.start(index, key),
            Command::OpenGate(key) => {
                // The gate surfaces (toast, card, needs-you count) come with the gates step; the
                // event alone sets the branch waiting on Kyle.
                let question = self.runs[index]
                    .definition
                    .node(&key.node)
                    .and_then(|node| node.gate.as_ref())
                    .map(|gate| gate.question.clone())
                    .unwrap_or_default();
                self.runs[index].append(Event::GateOpened { key, question });
            }
            Command::MergeBranch { join, branch, key } => {
                let run = &mut self.runs[index];
                match self.runner.merge_branch(&run.id, &join, &branch) {
                    Ok(sha) => run.append(Event::BranchMerged { join, branch, sha }),
                    Err(failure) => {
                        // The branches stay as they are. The conflict is a failed try of the
                        // join's key (scheduler.rs, `Command::MergeBranch`): the scheduler fails
                        // the branch, lets running keys finish and ends the run Failed once
                        // nothing runs. The conflict is terminal for the run: a retry step does
                        // not offer the join (`retry_set`), so only a fresh run recovers.
                        let failure = Failure::new(
                            FailureReason::MergeConflict,
                            Some(key.clone()),
                            failure.detail,
                        );
                        let try_number = run.next_try(&key);
                        run.append(Event::NodeStarted {
                            key: key.clone(),
                            try_number,
                            pid: 0,
                            pid_start_time: 0,
                        });
                        run.append(Event::NodeFinished {
                            key,
                            try_number,
                            result: None,
                            failure: Some(failure),
                            cost_usd: 0.0,
                            turns: None,
                            elapsed_ms: 0,
                            session_id: None,
                        });
                        return false;
                    }
                }
            }
            Command::CancelBranch { branch, .. } => self.cancel_branch(index, &branch),
            Command::Hold { key, failure } => {
                self.runs[index].append(Event::Held { key, failure });
            }
            Command::End {
                state: record::EndState::Succeeded,
                ..
            } if self.hold_success => {
                let id = self.runs[index].id.clone();
                if !self.settling.contains(&id) {
                    self.settling.push(id);
                }
                self.hold(index);
                return false;
            }
            Command::End { state, failure } => {
                self.runs[index].append(Event::Ended { state, failure });
            }
        }
        true
    }

    /// Starts a try of `key`. The process is spawned before its `NodeStarted` is appended, since
    /// the event records the pid: a crash between the two leaves a process no log names, which
    /// the next load cannot find (an orphan until it exits), and the key is not marked
    /// interrupted. The window is one `fork`/`exec` and one fsynced line; closing it would take a
    /// record written before the spawn (an intent line) and is not done in v1.
    ///
    /// A key of a registered built-in is started through it, with no process of its own: it is
    /// recorded with pid 0.
    fn start(&mut self, index: usize, key: NodeKey) {
        let run = &mut self.runs[index];
        let try_number = run.next_try(&key);
        let builtin = self.builtins.for_key(&run.definition, &key);
        let grant = self.consents.granted(&run.id);
        let started = match &builtin {
            Some(builtin) => {
                let state = run.state();
                // The runner knows the setting and the run's project, which can only tighten it.
                let mode = self.runner.unsandboxed_mode(&run.id);
                let permit = run.binding.as_ref().map_or(Permit::Refuse, |binding| {
                    crate::dispatch_program::program_permit(
                        &binding.setup.facts,
                        mode,
                        grant.as_ref(),
                    )
                });
                builtin
                    .start(BuiltinStart {
                        run: &run.id,
                        definition: &run.definition,
                        state: &state,
                        key: &key,
                        try_number,
                        binding: run.binding.as_ref(),
                        permit,
                        mode,
                    })
                    .map(|()| ProcessRecord {
                        pid: 0,
                        start_time: 0,
                    })
            }
            None => {
                let state = run.state();
                let budget_left_usd = self
                    .limits
                    .run_max_cost_usd
                    .map(|cap| (cap - state.cost_usd).max(0.0));
                self.runner.start_node(NodeStart {
                    run: &run.id,
                    definition: &run.definition,
                    key: &key,
                    try_number,
                    state: &state,
                    budget_left_usd,
                    binding: run.binding.as_ref(),
                    grant,
                })
            }
        };
        // Fail closed: a run nobody can answer for (unattended, a heal run, or one never bound)
        // never asks, whoever returned the ask.
        let askable = run.binding.as_ref().is_some_and(|binding| {
            crate::dispatch_program::program_permit(&binding.setup.facts, Mode::Ask, None)
                == Permit::Ask
        });
        let started = started.map_err(|failure| {
            if failure.reason == FailureReason::SandboxRequired && !askable {
                let reason = consent::probe_reason(&failure.detail).to_string();
                Failure::new(
                    FailureReason::SandboxRefused,
                    failure.node,
                    format!("{} ({reason})", unsandboxed::REFUSED_TEXT),
                )
            } else {
                failure
            }
        });
        match started {
            Err(failure) if failure.reason == FailureReason::SandboxRequired => {
                // Not a try: the key is held on the run's one ask (a later key joins the pending
                // one) until a person answers. The question names the node with
                // `unsandboxed::ask_text` over the probe's reason recorded here.
                let reason = consent::probe_reason(&failure.detail).to_string();
                log::info!(
                    "workflows: run {} asks to run {key} without the OS sandbox: {reason}",
                    run.id
                );
                run.append(Event::ConsentAsked { key, reason });
            }
            Ok(process) => {
                if let Some(builtin) = builtin {
                    run.builtins.insert(key.clone(), builtin);
                }
                run.append(Event::NodeStarted {
                    key: key.clone(),
                    try_number,
                    pid: process.pid,
                    pid_start_time: process.start_time,
                });
                run.in_flight.insert(key);
            }
            Err(failure) => {
                run.append(Event::NodeStarted {
                    key: key.clone(),
                    try_number,
                    pid: 0,
                    pid_start_time: 0,
                });
                run.append(Event::NodeFinished {
                    key,
                    try_number,
                    result: None,
                    failure: Some(failure),
                    cost_usd: 0.0,
                    turns: None,
                    elapsed_ms: 0,
                    session_id: None,
                });
            }
        }
    }

    /// A join `any` took another branch: stops the branch's tries and records each as
    /// interrupted and not resumed (the convention of the scheduler's fake).
    fn cancel_branch(&mut self, index: usize, branch: &Branch) {
        let run = &mut self.runs[index];
        let stopped: Vec<(NodeKey, u32)> = run
            .state()
            .running
            .iter()
            .filter(|(key, _)| key.branch.starts_with(branch))
            .map(|(key, running)| (key.clone(), running.try_number))
            .collect();
        let tries: Vec<(String, NodeKey)> = stopped
            .iter()
            .filter(|(key, _)| run.in_flight.remove(key))
            .map(|(key, _)| (run.id.clone(), key.clone()))
            .collect();
        self.stop_tries(tries);
        for (key, try_number) in stopped {
            let failure = Failure::new(
                FailureReason::InterruptedNotResumed,
                Some(key.clone()),
                "cancelled by a join any",
            );
            self.runs[index].append(Event::NodeFinished {
                key,
                try_number,
                result: None,
                failure: Some(failure),
                cost_usd: 0.0,
                turns: None,
                elapsed_ms: 0,
                session_id: None,
            });
        }
    }
}

/// The key whose finish took the run's cost to `cap` or past it, with the cost then; `None` with
/// no cap, no finished try, or a cost under the cap.
fn cap_crossing(state: &RunState, cap: Option<f64>) -> Option<(NodeKey, f64)> {
    let cap = cap?;
    if state.cost_usd < cap {
        return None;
    }
    let mut cost = 0.0;
    for finished in &state.finished {
        cost += finished.cost_usd;
        if cost >= cap {
            return Some((finished.key.clone(), state.cost_usd));
        }
    }
    None
}

fn process_record(running: &RunningNode) -> ProcessRecord {
    ProcessRecord {
        pid: running.pid,
        start_time: running.pid_start_time,
    }
}

/// Opens (or creates) the attempt's `events.jsonl`, repairs a tail cut by a crash, and reads it.
fn open_run(spec: RunSpec) -> Result<LiveRun, RuntimeError> {
    std::fs::create_dir_all(&spec.attempt_dir)
        .map_err(|error| RuntimeError::io(&spec.attempt_dir, error))?;
    let path = spec.attempt_dir.join(EVENTS_FILE);
    let mut log = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| RuntimeError::io(&path, error))?;
    let parsed = record::repair(&mut log).map_err(|error| RuntimeError::io(&path, error))?;
    if parsed.dropped_tail {
        log::warn!(
            "workflows: dropped a cut last line of {} (a crash mid-write)",
            path.display()
        );
    }
    Ok(LiveRun {
        id: spec.id,
        definition: spec.definition,
        log,
        events: parsed.events,
        status: Status::Held,
        in_flight: BTreeSet::new(),
        builtins: BTreeMap::new(),
        binding: None,
        loop_bound: spec.loop_bound,
        started_by: StartedBy::Person,
    })
}

/// Whether a try of `key` holds a session slot: every node's but a checks node's, which pools
/// govern instead.
pub fn holds_session(definition: &Definition, key: &NodeKey) -> bool {
    !definition
        .node(&key.node)
        .is_some_and(bongos_workflow_model::definition::Node::is_checks)
}

/// The scripted runner of the tests.
#[cfg(test)]
#[path = "runtime_fake.rs"]
pub(crate) mod fake;
