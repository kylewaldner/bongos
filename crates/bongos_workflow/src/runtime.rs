//! The engine runtime (feature 20, D12): the run loop over [`scheduler::step`], the run, session
//! and branch limits, the Queued state, halt, cancel, quit and the load after a relaunch.
//!
//! The runtime owns the live runs, each with its open `events.jsonl` (D5). [`Runtime::tick`] takes
//! the finished tries from the [`Runner`], appends them, then asks [`scheduler::step`] what each
//! executing run does next and carries it out, appending one event per command, until nothing
//! changes. The run state is never kept apart from the log: it is [`replay`] of the events.
//!
//! Limits ([`Limits`], from the `workflows` settings):
//! - Runs: at most `max_concurrent_runs` execute at once. The rest wait Queued in start order (a
//!   `Queued` event) and start when a slot frees. A run holds its slot while it has a process
//!   running or something to do now; gates, sleeping polls and Queued nodes hold no slot (D12), so
//!   a run whose only live work waits on those is parked and frees its slot. A parked run that has
//!   work again (a gate closed, a poll is due) waits for a slot behind the runs already queued,
//!   without a new `Queued` event. A run held for Kyle (ended, cancelled, halted, interrupted)
//!   holds none either.
//! - Sessions: the processes executing across all runs never exceed the session limit; the free
//!   count is the `free` of [`scheduler::step`]. Gates, sleeping polls and Queued runs hold none.
//! - Branches: the scheduler keeps a run's executing branches within `max_parallel_branches`.
//!
//! Halt and cancel stop the run's processes through [`Runner::stop`], within 5 s (D12). Quit stops
//! every process and appends `Interrupted` with the keys in flight. [`Runtime::load`] never resumes
//! a run by itself: it stops processes a crash left behind (only those whose pid and start time
//! both match the record), marks a run with a started key and no finish interrupted, and gives an
//! interrupted, halted, ended or cancelled run no command until Kyle acts.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use bongos_workflow_model::record::RunningNode;
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{
    Command, Definition, Event, Failure, FailureReason, Limits, NodeKey, RunState, record, replay,
    scheduler,
};
use serde_json::Value;

use crate::WorkflowsSettings;
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

/// What runs node tries: the agent runners, the program runner and the built-ins, or a fake in
/// tests. The runtime decides; the runner only starts, reports and stops.
pub trait Runner {
    /// Whether this runner can execute nodes. The app's stand-in says no, so a start from the app
    /// refuses before it creates anything.
    fn is_connected(&self) -> bool {
        true
    }

    /// Starts try `try_number` of `key` of run `run`; the process it started, or why it could not.
    fn start(
        &mut self,
        run: &str,
        definition: &Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<ProcessRecord, Failure>;

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
    fn is_connected(&self) -> bool {
        (**self).is_connected()
    }

    fn start(
        &mut self,
        run: &str,
        definition: &Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        (**self).start(run, definition, key, try_number)
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
    /// Tries this runtime started whose finish it has not seen: the sessions the run holds.
    in_flight: BTreeSet<NodeKey>,
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
        state.cancelled || state.ended.is_some() || state.halted || !state.interrupted.is_empty()
    }
}

/// Why a runtime command was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error("no run {0} is loaded")]
    UnknownRun(String),
    #[error("run {0} is over")]
    Over(String),
    #[error("run {0} is already loaded")]
    Duplicate(String),
    #[error("{0}")]
    Io(String),
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
        }
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
        let ends_succeeded =
            scheduler::step(&self.runs[index].definition, &state, &self.limits, free)
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

    /// Whether run `id` is waiting for a run slot.
    pub fn is_queued(&self, id: &str) -> bool {
        self.queue.iter().any(|queued| queued == id)
    }

    /// The processes executing across all runs.
    pub fn sessions_in_flight(&self) -> usize {
        self.runs.iter().map(|run| run.in_flight.len()).sum()
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

    /// Loads a run after a relaunch; [`Runtime::load_all`] with one run.
    pub fn load(&mut self, spec: RunSpec) -> Result<Vec<(NodeKey, OrphanOutcome)>, RuntimeError> {
        let mut orphans = self.load_all(vec![spec])?;
        Ok(orphans.pop().unwrap_or_default())
    }

    /// Loads runs after a relaunch. First every process the logs record as running (or
    /// interrupted with a process) is stopped when its pid is alive with the recorded start time,
    /// all runs' processes together in one shared grace period; a pid with another start time is
    /// a different process and is left alone. Then a run with a started key and no finish is
    /// marked interrupted with every such key. An interrupted, halted, ended or cancelled run gets
    /// no command until Kyle acts; any other run (queued, at a gate, with a sleeping poll) waits
    /// for a run slot like a new one. Nothing starts here. A duplicate id or a log that cannot be
    /// opened loads none of them. Returns each run's orphan outcomes, in `specs` order.
    pub fn load_all(
        &mut self,
        specs: Vec<RunSpec>,
    ) -> Result<Vec<Vec<(NodeKey, OrphanOutcome)>>, RuntimeError> {
        let mut runs: Vec<LiveRun> = Vec::with_capacity(specs.len());
        for spec in specs {
            if self.find(&spec.id).is_some() || runs.iter().any(|run| run.id == spec.id) {
                return Err(RuntimeError::Duplicate(spec.id));
            }
            runs.push(open_run(spec)?);
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
            if LiveRun::is_held(&state) {
                run.status = Status::Held;
            } else {
                run.status = Status::Queued;
                self.queue.push_back(run.id.clone());
            }
            self.runs.push(run);
        }
        Ok(orphans)
    }

    /// One pass of the run loop: appends the tries that ended, admits queued runs while run slots
    /// are free, and carries out what [`scheduler::step`] gives each executing run, until nothing
    /// changes. Whether anything changed.
    pub fn tick(&mut self) -> bool {
        if self.quitting {
            return false;
        }
        let mut changed = self.take_finishes();
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
        self.hold(index);
        Ok(())
    }

    /// Cancels run `id` (also closing out a Failed run): stops its processes within 5 s and
    /// appends `Cancelled`.
    pub fn cancel(&mut self, id: &str) -> Result<(), RuntimeError> {
        let index = self.live(id)?;
        self.stop_in_flight(&[index]);
        self.runs[index].append(Event::Cancelled);
        self.hold(index);
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

    /// Resumes run `id`: appends `Resumed` when it was paused or halted, then starts a new try of
    /// every key a halt stopped or a quit interrupted ([`scheduler::resume_set`] less the keys
    /// still in flight), in the same worktree. A resumed try does not count against `retries`,
    /// and completed keys are not rerun. The run then waits for a run slot. The keys started.
    pub fn resume(&mut self, id: &str) -> Result<Vec<NodeKey>, RuntimeError> {
        let index = self.live(id)?;
        let state = self.runs[index].state();
        if state.paused || state.halted {
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
        self.queue.retain(|queued| queued != id);
        self.settling.retain(|settling| settling != id);
        self.runs.remove(index);
        true
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
        let indices: Vec<usize> = (0..self.runs.len()).collect();
        self.stop_in_flight(&indices);
        for run in &mut self.runs {
            let state = run.state();
            if state.running.is_empty() || state.halted || state.is_over() {
                continue;
            }
            let keys: Vec<NodeKey> = state.running.keys().cloned().collect();
            run.append(Event::Interrupted { keys });
            run.status = Status::Held;
        }
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
        self.queue.retain(|queued| *queued != id);
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
        if !tries.is_empty() {
            self.runner.stop(&tries);
        }
    }

    fn take_finishes(&mut self) -> bool {
        let mut changed = false;
        for finish in self.runner.poll() {
            let Some(index) = self.find(&finish.run) else {
                log::warn!("workflows: a try of unknown run {} finished", finish.run);
                continue;
            };
            let run = &mut self.runs[index];
            if !run.in_flight.remove(&finish.key) {
                // Stopped by a halt, a cancel or a join `any`: what it reports does not count.
                continue;
            }
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

    /// Moves queued runs to executing, oldest first, while run slots are free, and appends
    /// `Queued` to each run left waiting that does not have it yet.
    fn admit_queued(&mut self) -> bool {
        let mut changed = false;
        loop {
            let executing = self
                .runs
                .iter()
                .filter(|run| run.status == Status::Executing)
                .count();
            if !self.limits.admits_run(executing) {
                break;
            }
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            if let Some(index) = self.find(&id) {
                self.runs[index].status = Status::Executing;
                changed = true;
            }
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
            let free = self.limits.free_sessions(self.sessions_in_flight());
            if !scheduler::step(&self.runs[index].definition, &state, &self.limits, free).is_empty()
            {
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
            let free = self.limits.free_sessions(self.sessions_in_flight());
            let commands =
                scheduler::step(&self.runs[index].definition, &state, &self.limits, free);
            if commands.is_empty() {
                if state.running.is_empty() && self.runs[index].in_flight.is_empty() {
                    self.runs[index].status = Status::Parked;
                    changed = true;
                }
                continue;
            }
            for command in commands {
                changed = true;
                if !self.apply(index, command) {
                    // A merge failed: the rest of this step (more merges, starts) was planned
                    // before it, so it is dropped; the next round asks the scheduler again.
                    break;
                }
                if LiveRun::is_held(&self.runs[index].state()) {
                    break;
                }
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
    fn start(&mut self, index: usize, key: NodeKey) {
        let run = &mut self.runs[index];
        let try_number = run.next_try(&key);
        match self
            .runner
            .start(&run.id, &run.definition, &key, try_number)
        {
            Ok(process) => {
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
        if !tries.is_empty() {
            self.runner.stop(&tries);
        }
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
    })
}

/// A scripted [`Runner`] for tests. Each start takes the node's scripted result (or the default
/// `{"summary": "ok"}`); a try finishes when the test releases it, or at the next poll with
/// `auto_finish`. With `command` set, each start spawns that command as a real process in its own
/// session ([`NodeProcess`](crate::process::NodeProcess)), so halt, cancel and quit stop real
/// processes.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::BTreeMap;
    use std::process::{Command, Stdio};

    use bongos_workflow_model::{Definition, Failure, FailureReason, NodeKey};
    use serde_json::{Value, json};

    use super::{Finish, Runner};
    use crate::process::{NodeProcess, ProcessRecord, stop_all};
    use bongos_workflow_model::scheduler::Branch;

    struct Try {
        run: String,
        key: NodeKey,
        try_number: u32,
        process: Option<NodeProcess>,
        released: bool,
    }

    #[derive(Default)]
    pub struct FakeRunner {
        /// Finish every try at the next poll.
        pub auto_finish: bool,
        /// A command each start spawns as a real process.
        pub command: Option<Vec<String>>,
        /// Every try started, in order, as `(run, key)`.
        pub started: Vec<(String, NodeKey)>,
        /// Every try stopped, in order.
        pub stopped: Vec<(String, NodeKey)>,
        /// The processes stopped, kept so a test can check they are gone.
        pub stopped_processes: Vec<NodeProcess>,
        /// The most tries in flight at once, across runs.
        pub max_in_flight: usize,
        /// Every merge fails `merge conflict` when set, after the first `clean_merges` merges.
        pub merge_conflicts: bool,
        /// How many merges succeed before `merge_conflicts` takes effect.
        pub clean_merges: usize,
        /// Every merge asked for, as `(run, join, branch)`.
        pub merges: Vec<(String, String, Branch)>,
        results: BTreeMap<String, Value>,
        /// Tries of each node still to fail, `agent exited nonzero`.
        failures: BTreeMap<String, u32>,
        /// The session id each node's tries report.
        sessions: BTreeMap<String, String>,
        running: Vec<Try>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// The result every try of `node` finishes with.
        pub fn script(&mut self, node: &str, result: Value) -> &mut Self {
            self.results.insert(node.to_string(), result);
            self
        }

        /// The session id every try of `node` reports when it finishes.
        pub fn session(&mut self, node: &str, id: &str) -> &mut Self {
            self.sessions.insert(node.to_string(), id.to_string());
            self
        }

        /// The next `times` tries of `node` fail `agent exited nonzero`.
        pub fn fail(&mut self, node: &str, times: u32) -> &mut Self {
            self.failures.insert(node.to_string(), times);
            self
        }

        /// The tries in flight, as `(run, key)`.
        pub fn in_flight(&self) -> Vec<(String, NodeKey)> {
            self.running
                .iter()
                .map(|running| (running.run.clone(), running.key.clone()))
                .collect()
        }

        /// The processes of the tries in flight.
        pub fn processes(&mut self) -> impl Iterator<Item = &mut NodeProcess> {
            self.running
                .iter_mut()
                .filter_map(|running| running.process.as_mut())
        }

        /// Lets every try in flight finish at the next poll.
        pub fn release_all(&mut self) {
            for running in &mut self.running {
                running.released = true;
            }
        }

        /// Lets the tries of run `run` finish at the next poll.
        pub fn release(&mut self, run: &str) {
            for running in &mut self.running {
                if running.run == run {
                    running.released = true;
                }
            }
        }
    }

    impl Runner for FakeRunner {
        fn start(
            &mut self,
            run: &str,
            _definition: &Definition,
            key: &NodeKey,
            try_number: u32,
        ) -> Result<ProcessRecord, Failure> {
            let process = self.command.as_ref().map(|argv| {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                NodeProcess::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
                    .expect("the fake command starts")
            });
            let record = process.as_ref().map_or(
                ProcessRecord {
                    pid: 4000 + self.started.len() as u32,
                    start_time: 0,
                },
                NodeProcess::record,
            );
            self.started.push((run.to_string(), key.clone()));
            self.running.push(Try {
                run: run.to_string(),
                key: key.clone(),
                try_number,
                process,
                released: false,
            });
            self.max_in_flight = self.max_in_flight.max(self.running.len());
            Ok(record)
        }

        fn poll(&mut self) -> Vec<Finish> {
            let auto_finish = self.auto_finish;
            let (done, running): (Vec<Try>, Vec<Try>) = std::mem::take(&mut self.running)
                .into_iter()
                .partition(|running| auto_finish || running.released);
            self.running = running;
            done.into_iter()
                .map(|mut done| {
                    if let Some(process) = done.process.as_mut() {
                        process.stop();
                    }
                    if let Some(left) = self.failures.get_mut(&done.key.node)
                        && *left > 0
                    {
                        *left -= 1;
                        let failure = Failure::new(
                            FailureReason::AgentExitedNonzero,
                            Some(done.key.clone()),
                            "scripted failure",
                        );
                        return Finish {
                            outcome: Err(failure),
                            ..Finish::ok(done.run, done.key, done.try_number, Value::Null)
                        };
                    }
                    let result = self
                        .results
                        .get(&done.key.node)
                        .cloned()
                        .unwrap_or_else(|| json!({ "summary": "ok" }));
                    let session_id = self.sessions.get(&done.key.node).cloned();
                    Finish {
                        session_id,
                        ..Finish::ok(done.run, done.key, done.try_number, result)
                    }
                })
                .collect()
        }

        fn stop(&mut self, tries: &[(String, NodeKey)]) {
            let (stopped, running): (Vec<Try>, Vec<Try>) = std::mem::take(&mut self.running)
                .into_iter()
                .partition(|running| {
                    tries
                        .iter()
                        .any(|(run, key)| *run == running.run && *key == running.key)
                });
            self.running = running;
            let mut processes = Vec::new();
            for stopped in stopped {
                self.stopped.push((stopped.run, stopped.key));
                processes.extend(stopped.process);
            }
            stop_all(&mut processes);
            self.stopped_processes.extend(processes);
        }

        fn merge_branch(
            &mut self,
            run: &str,
            join: &str,
            branch: &Branch,
        ) -> Result<String, Failure> {
            self.merges
                .push((run.to_string(), join.to_string(), branch.clone()));
            if self.merge_conflicts && self.merges.len() > self.clean_merges {
                return Err(Failure::new(
                    FailureReason::MergeConflict,
                    None,
                    "conflict in src/lib.rs",
                ));
            }
            Ok("0000000".to_string())
        }
    }
}
