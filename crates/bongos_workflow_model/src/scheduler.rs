//! The scheduler: what a run does next, decided from its definition and its replayed state.
//!
//! [`step`] is pure. It walks the graph from `start` over the recorded results (never over
//! `RouteTaken` events, which are for display), so the same log always gives the same commands and
//! a command already carried out is not given again: a started key is running, an opened gate is
//! open, a merged branch is merged. The engine carries out each [`Command`], appends the matching
//! event, replays and calls [`step`] again.
//!
//! The rules, in the words of the product and its design:
//! - Routing runs only the routed path: the first route whose `when` all match the typed result (a
//!   route with no `when` always matches), else the `default` route, else the run fails `no route
//!   for a result` at that node.
//! - Fan-out: `to = [a, b]` starts one branch per listed node; `fan_out = { over, to, width }` one
//!   branch of `to` per entry of the list field. A list longer than `width` (default
//!   [`Limits::fan_out_width`]) fails the fan-out node `branch limit exceeded`. Branch `i` of fan
//!   node `f` adds `(f, i)` to the key's branch.
//! - Joins: a branch that routes to a node with `join` arrives there. `all` waits until every
//!   branch has arrived or ended, merges the arrived ones in branch order and runs the join once;
//!   `any` takes the first arrival in log order, cancels the branches still going and merges only
//!   the winner.
//! - Passes: each instance of a node within one branch gets the next pass (1, 2, ...). A route with
//!   `max_passes = n` is taken only while its target's next pass is at most `n`; past that it
//!   follows `on_bound` (a node or an end), or fails `loop bound reached` when `on_bound` is
//!   absent or `"fail"`.
//! - Retries: a failed try is rerun as a new try of the same key while the failures since the last
//!   retry step are at most the node's `retries` (node, then `[defaults]`, then
//!   [`Limits::retries`]). Failures that a rerun cannot change are never retried (see
//!   [`is_retried`]).
//! - Failure rule: once a branch has failed (retries spent, a derived failure, or a failure end),
//!   nothing new starts, gates do not open and branches do not merge; running keys finish; when
//!   nothing runs the run ends Failed with the earliest failure in log order.
//! - Success rule: when nothing is left to do, the run succeeds if it reached at least one end,
//!   none of them a failure end, no branch failed, and every outcome promised by the success ends
//!   reached was delivered by a field marked `outcome` of a completed key; otherwise it fails
//!   `outcome promised but not delivered`.
//! - Limits: starts never exceed `free` (free session slots across runs, which the engine counts),
//!   and the branches of one run that have a running key never exceed
//!   [`Limits::max_parallel_branches`]. Gates, sleeping polls and keys waiting their turn hold no
//!   slot. Runs at once ([`Limits::max_concurrent_runs`]) are the engine's queue; [`Limits`]
//!   offers the arithmetic.
//! - A run that is halted, interrupted (an `Interrupted` key not started again), ended or
//!   cancelled gets no commands: it waits for Kyle ([`resume_set`], [`retry_set`]). A paused run
//!   starts nothing, but still cancels, and ends once nothing is left to do.

use std::collections::{BTreeSet, HashMap, VecDeque};

use serde_json::Value;

use crate::definition::{
    Definition, EndKind, FanOut, FieldType, GateWhen, JoinWait, Node, Outcome, Route, Target,
};
use crate::failure::{Failure, FailureReason};
use crate::record::{EndState, NodeKey, RunState};
use crate::schema::{self, TypedResult, TypedValue};

/// A key's fan-out branch: `(fan node, item index)` pairs, outermost first.
pub type Branch = Vec<(String, u32)>;

/// `on_bound` value that fails the run (also the default).
const ON_BOUND_FAIL: &str = "fail";

/// The engine's limits, all settings (D19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `workflows.max_concurrent_runs`: runs executing at once; the rest are Queued.
    pub max_concurrent_runs: u32,
    /// `workflows.max_concurrent_sessions`: agent sessions plus programs executing across all
    /// runs; `None` is twice the run limit.
    pub max_concurrent_sessions: Option<u32>,
    /// `workflows.max_parallel_branches`: branches of one run executing at once.
    pub max_parallel_branches: u32,
    /// `workflows.fan_out_width`: the widest per-item fan-out when a route sets no `width`.
    pub fan_out_width: u32,
    /// `workflows.retries`: automatic retries of a node that sets none.
    pub retries: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 3,
            max_concurrent_sessions: None,
            max_parallel_branches: 3,
            fan_out_width: 8,
            retries: 2,
        }
    }
}

impl Limits {
    /// Sessions that may execute at once across all runs.
    pub fn session_limit(&self) -> usize {
        self.max_concurrent_sessions
            .unwrap_or(self.max_concurrent_runs.saturating_mul(2)) as usize
    }

    /// The `free` argument of [`step`] when `running` sessions execute across all runs.
    pub fn free_sessions(&self, running: usize) -> usize {
        self.session_limit().saturating_sub(running)
    }

    /// Whether one more run may start executing while `executing` runs do; otherwise it is Queued.
    pub fn admits_run(&self, executing: usize) -> bool {
        executing < self.max_concurrent_runs as usize
    }
}

/// What the engine does next for one run. Every command is idempotent: [`step`] gives it again
/// until the event it leads to is in the log.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Start a try of the key (`NodeStarted`); the try number is one more than its earlier tries.
    Start(NodeKey),
    /// Open the key's gate (`GateOpened`) with the node's question.
    OpenGate(NodeKey),
    /// Merge a finished branch into the run branch (`BranchMerged`) before `join` runs. A
    /// conflict is recorded as a `NodeFinished` of the join's key failed `merge conflict`.
    MergeBranch { join: String, branch: Branch },
    /// Stop every running key of the branch: a join `any` took another branch. Whatever the
    /// stopped tries record is ignored, since the branch no longer counts.
    CancelBranch { join: String, branch: Branch },
    /// Append `Ended`. A failure end of the definition ends with `failure: None`; the end and the
    /// key that reached it are in [`Plan::failed`].
    End {
        state: EndState,
        failure: Option<Failure>,
    },
}

/// A failed branch.
#[derive(Debug, Clone, PartialEq)]
pub enum Failed {
    /// A node failed for good, or routing from its result failed.
    Node(Failure),
    /// The branch reached a failure end of the definition.
    End { end: String, from: NodeKey },
}

impl Failed {
    /// The failure the `Ended` event records.
    pub fn failure(&self) -> Option<Failure> {
        match self {
            Failed::Node(failure) => Some(failure.clone()),
            Failed::End { .. } => None,
        }
    }

    /// The key to rerun for a retry step.
    pub fn key(&self) -> Option<&NodeKey> {
        match self {
            Failed::Node(failure) => failure.node.as_ref(),
            Failed::End { from, .. } => Some(from),
        }
    }
}

/// An end a branch reached.
#[derive(Debug, Clone, PartialEq)]
pub struct ReachedEnd {
    pub end: String,
    pub kind: EndKind,
    /// The key whose result routed there.
    pub from: NodeKey,
    pub promised: Vec<Outcome>,
}

/// Where every live branch of a run stands. Keys of a branch cancelled by a join `any` appear only
/// in `cancelling` (while they still run); nothing else of theirs counts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Keys to start: a first try, an automatic retry or a revision, in branch order.
    pub ready: Vec<NodeKey>,
    pub running: Vec<NodeKey>,
    /// Keys whose gate must open (before the first try, or after a result).
    pub gates_to_open: Vec<NodeKey>,
    /// Keys waiting on an open gate.
    pub open_gates: Vec<NodeKey>,
    /// Polling keys whose last result was the `polls` value: the engine reruns them on its timer.
    pub sleeping: Vec<NodeKey>,
    /// Keys interrupted and not started again: the run waits for a resume.
    pub interrupted: Vec<NodeKey>,
    /// Branches to merge before their join runs, as `(join node, branch)`.
    pub merges: Vec<(String, Branch)>,
    /// Cancelled branches that still have running keys, as `(join node, branch)`.
    pub cancels: Vec<(String, Branch)>,
    /// Running keys of cancelled branches.
    pub cancelling: Vec<NodeKey>,
    /// Failed branches, earliest in log order first.
    pub failed: Vec<Failed>,
    pub ends: Vec<ReachedEnd>,
    /// Outcomes delivered by completed keys (non-empty values of `outcome` fields).
    pub delivered: Vec<Outcome>,
    /// Nodes the walk reached, cancelled branches included.
    pub visited: BTreeSet<String>,
}

impl Plan {
    /// The failure that ends the run: the earliest.
    pub fn failure(&self) -> Option<&Failed> {
        self.failed.first()
    }

    /// Whether nothing is left to do: nothing ready, running, gated, sleeping, interrupted,
    /// merging or cancelling.
    pub fn is_idle(&self) -> bool {
        self.ready.is_empty()
            && self.running.is_empty()
            && self.gates_to_open.is_empty()
            && self.open_gates.is_empty()
            && self.sleeping.is_empty()
            && self.interrupted.is_empty()
            && self.merges.is_empty()
            && self.cancelling.is_empty()
    }

    /// Outcomes promised by the success ends reached and not delivered, with the end promising
    /// each.
    pub fn missing_outcomes(&self) -> Vec<(Outcome, &ReachedEnd)> {
        let mut missing: Vec<(Outcome, &ReachedEnd)> = Vec::new();
        for end in self.ends.iter().filter(|end| end.kind == EndKind::Success) {
            for outcome in &end.promised {
                let listed = missing.iter().any(|(listed, _)| listed == outcome);
                if !self.delivered.contains(outcome) && !listed {
                    missing.push((*outcome, end));
                }
            }
        }
        missing
    }

    /// Nodes of the definition the run never reached: the paths not taken, shown skipped once the
    /// run ends.
    pub fn skipped<'d>(&self, definition: &'d Definition) -> Vec<&'d str> {
        definition
            .nodes
            .iter()
            .filter(|node| !self.visited.contains(&node.name))
            .map(|node| node.name.as_str())
            .collect()
    }
}

/// What the run does next. `free` is the number of session slots free across all runs
/// ([`Limits::free_sessions`]); at most that many keys start.
pub fn step(
    definition: &Definition,
    state: &RunState,
    limits: &Limits,
    free: usize,
) -> Vec<Command> {
    if state.cancelled || state.ended.is_some() || state.halted {
        return Vec::new();
    }
    let plan = plan(definition, state, limits);
    if !plan.interrupted.is_empty() {
        return Vec::new();
    }
    let mut commands: Vec<Command> = plan
        .cancels
        .iter()
        .map(|(join, branch)| Command::CancelBranch {
            join: join.clone(),
            branch: branch.clone(),
        })
        .collect();
    let nothing_runs = state.running.is_empty();
    if let Some(failed) = plan.failure() {
        if nothing_runs {
            commands.push(Command::End {
                state: EndState::Failed,
                failure: failed.failure(),
            });
        }
        return commands;
    }
    if !state.paused {
        commands.extend(
            plan.merges
                .iter()
                .map(|(join, branch)| Command::MergeBranch {
                    join: join.clone(),
                    branch: branch.clone(),
                }),
        );
        commands.extend(plan.gates_to_open.iter().cloned().map(Command::OpenGate));
        commands.extend(starts(&plan, limits, free));
    }
    if commands.is_empty() && plan.is_idle() && nothing_runs {
        commands.push(finish(&plan));
    }
    commands
}

/// The keys to start now, within `free` and the per-run branch limit.
fn starts(plan: &Plan, limits: &Limits, free: usize) -> Vec<Command> {
    let mut executing: BTreeSet<&Branch> = plan
        .running
        .iter()
        .chain(&plan.cancelling)
        .filter(|key| !key.branch.is_empty())
        .map(|key| &key.branch)
        .collect();
    let mut commands = Vec::new();
    for key in &plan.ready {
        if commands.len() >= free {
            break;
        }
        if !key.branch.is_empty() && !executing.contains(&key.branch) {
            if executing.len() >= limits.max_parallel_branches as usize {
                continue;
            }
            executing.insert(&key.branch);
        }
        commands.push(Command::Start(key.clone()));
    }
    commands
}

/// The `End` of a run with nothing left to do and no failed branch.
fn finish(plan: &Plan) -> Command {
    let failed = |failure| Command::End {
        state: EndState::Failed,
        failure: Some(failure),
    };
    if plan.ends.is_empty() {
        return failed(Failure::new(
            FailureReason::NoRouteForResult,
            None,
            "the run reached no end",
        ));
    }
    if let Some((outcome, end)) = plan.missing_outcomes().first() {
        return failed(Failure::new(
            FailureReason::OutcomePromisedNotDelivered,
            Some(end.from.clone()),
            format!(
                "the success end '{}' promises {}, which no node delivered",
                end.end,
                outcome_name(*outcome)
            ),
        ));
    }
    Command::End {
        state: EndState::Succeeded,
        failure: None,
    }
}

fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pr => "pr",
        Outcome::Document => "document",
        Outcome::Tasks => "tasks",
    }
}

/// Where every branch of the run stands ([`step`] without the limits).
pub fn plan(definition: &Definition, state: &RunState, limits: &Limits) -> Plan {
    let mut walk = Walk::new(definition, state, limits);
    walk.run();
    walk.finish()
}

/// The keys a resume reruns, each as a new try that does not count against `retries`: every key
/// interrupted or still recorded running (a halt). Completed keys are never in it, and a key
/// waiting on an open gate stays a gate.
pub fn resume_set(state: &RunState) -> Vec<NodeKey> {
    let keys: BTreeSet<&NodeKey> = state
        .running
        .keys()
        .chain(state.interrupted.keys())
        .filter(|key| !state.open_gates.contains_key(*key))
        .collect();
    keys.into_iter().cloned().collect()
}

/// The keys a retry step may rerun: one per failed branch, earliest first. Everything else keeps
/// its recorded result, finished sibling branches included. The engine reruns one (a picker when
/// several failed) by starting it; that start reopens the failed run and gives the key its
/// retries again ([`RunState::retry_floors`]).
pub fn retry_set(definition: &Definition, state: &RunState, limits: &Limits) -> Vec<NodeKey> {
    let mut keys: Vec<NodeKey> = Vec::new();
    for failed in plan(definition, state, limits).failed {
        if let Some(key) = failed.key()
            && !keys.contains(key)
        {
            keys.push(key.clone());
        }
    }
    keys
}

/// Why a retry step with a fresh definition is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FreshRetryRefusal(pub String);

/// Whether a retry step may continue the run on `new` in place of `old`, reusing every completed
/// result. Refused when a node the run completed is gone, or one of its output fields is gone,
/// renamed or of another type (a choice may gain values but not lose any), or it gains a required
/// field (its recorded results would not have it).
pub fn check_fresh_retry(
    old: &Definition,
    new: &Definition,
    state: &RunState,
) -> Result<(), FreshRetryRefusal> {
    let completed: BTreeSet<&str> = state
        .finished
        .iter()
        .filter(|finished| finished.succeeded())
        .map(|finished| finished.key.node.as_str())
        .collect();
    for name in completed {
        let refuse = |what: String| {
            Err(FreshRetryRefusal(format!(
                "the new definition {what}, which this run completed; restart instead"
            )))
        };
        let Some(new_node) = new.node(name) else {
            return refuse(format!("drops node '{name}'"));
        };
        let Some(old_node) = old.node(name) else {
            continue;
        };
        for field in &old_node.output {
            match new_node.output.iter().find(|new| new.name == field.name) {
                None => return refuse(format!("drops output field '{}' of '{name}'", field.name)),
                Some(new) if !compatible(&field.field_type, &new.field_type) => {
                    return refuse(format!("changes output field '{}' of '{name}'", field.name));
                }
                Some(_) => {}
            }
        }
        for field in &new_node.output {
            let known = old_node.output.iter().any(|old| old.name == field.name);
            if !known && !field.optional {
                return refuse(format!(
                    "adds required output field '{}' to '{name}'",
                    field.name
                ));
            }
        }
    }
    Ok(())
}

fn compatible(old: &FieldType, new: &FieldType) -> bool {
    match (old, new) {
        (FieldType::Choice(old), FieldType::Choice(new)) => {
            old.iter().all(|value| new.contains(value))
        }
        _ => old == new,
    }
}

/// Whether a failure with this reason is retried automatically. Not retried: failures a rerun of
/// the same key cannot change (routing, bounds, width, outcomes, a merge conflict), and those that
/// are Kyle's or the engine's decision (a cancelled gate, an interruption not resumed).
pub fn is_retried(reason: FailureReason) -> bool {
    !matches!(
        reason,
        FailureReason::NoRouteForResult
            | FailureReason::LoopBoundReached
            | FailureReason::BranchLimitExceeded
            | FailureReason::OutcomePromisedNotDelivered
            | FailureReason::MergeConflict
            | FailureReason::GateCancelled
            | FailureReason::InterruptedNotResumed
    )
}

/// The node's automatic retries.
fn retries_of(definition: &Definition, node: &Node, limits: &Limits) -> usize {
    node.settings
        .retries
        .or(definition.defaults.retries)
        .unwrap_or(limits.retries) as usize
}

/// `(fan id, branch index)` pairs from the outermost fan-out in.
type Scope = Vec<(usize, usize)>;

/// One line of execution: a key to look at and the log position that led to it.
struct Chain {
    key: NodeKey,
    scope: Scope,
    /// The index in `RunState::finished` of the finish that routed here (0 for `start`).
    time: usize,
}

#[derive(Debug, Clone)]
enum BranchEnd {
    Open,
    Arrived { join: String, time: usize },
    Ended { time: usize },
}

impl BranchEnd {
    fn is_open(&self) -> bool {
        matches!(self, BranchEnd::Open)
    }
}

/// One fan-out instance.
struct Fan {
    key: NodeKey,
    scope: Scope,
    time: usize,
    /// For a per-item fan-out, its `to`: an empty list continues at the join found from it.
    seed: Option<String>,
    branches: Vec<BranchEnd>,
    resolved: bool,
}

/// What one chain stopped at.
enum Item {
    Ready(NodeKey),
    Running(NodeKey),
    OpenGate(NodeKey),
    Gated(NodeKey),
    Sleeping(NodeKey),
    Interrupted(NodeKey),
    Merge(String, Branch),
    Failed(usize, Failed),
    End(ReachedEnd),
    Delivered(Vec<Outcome>),
}

/// A key's state from the log.
enum Status {
    Running,
    Interrupted,
    NeedsGate,
    Gated,
    Ready,
    Sleeping,
    Failed(usize, Failure),
    Done(usize, TypedResult),
}

struct Walk<'a> {
    definition: &'a Definition,
    state: &'a RunState,
    limits: &'a Limits,
    /// Indices into `state.finished` per key, in log order.
    finishes: HashMap<&'a NodeKey, Vec<usize>>,
    /// The last pass given to each `(node, branch)`.
    passes: HashMap<(String, Branch), u32>,
    /// How many joins so far merge each `(join, branch)`, to match against `state.merged`.
    merge_uses: HashMap<(String, Branch), usize>,
    fans: Vec<Fan>,
    items: Vec<(Scope, Item)>,
    queue: VecDeque<Chain>,
    /// Branches cancelled by a join `any`, with that join, in the order cancelled.
    cancelled: Vec<((usize, usize), String)>,
    visited: BTreeSet<String>,
}

impl<'a> Walk<'a> {
    fn new(definition: &'a Definition, state: &'a RunState, limits: &'a Limits) -> Self {
        let mut finishes: HashMap<&NodeKey, Vec<usize>> = HashMap::new();
        for (index, finished) in state.finished.iter().enumerate() {
            finishes.entry(&finished.key).or_default().push(index);
        }
        Self {
            definition,
            state,
            limits,
            finishes,
            passes: HashMap::new(),
            merge_uses: HashMap::new(),
            fans: Vec::new(),
            items: Vec::new(),
            queue: VecDeque::new(),
            cancelled: Vec::new(),
            visited: BTreeSet::new(),
        }
    }

    fn run(&mut self) {
        let start = self.definition.start.value.clone();
        if let Some(chain) = self.enter(Vec::new(), Vec::new(), None, &start, 0) {
            self.queue.push_back(chain);
        }
        loop {
            while let Some(chain) = self.queue.pop_front() {
                self.walk(chain);
            }
            let mut progressed = false;
            for id in 0..self.fans.len() {
                if !self.fans[id].resolved {
                    progressed |= self.resolve(id);
                }
            }
            if !progressed {
                break;
            }
        }
    }

    fn next_pass(&self, node: &str, branch: &Branch) -> u32 {
        self.passes
            .get(&(node.to_string(), branch.clone()))
            .copied()
            .unwrap_or(0)
            + 1
    }

    fn next_key(&mut self, node: &str, branch: Branch) -> NodeKey {
        let pass = self
            .passes
            .entry((node.to_string(), branch.clone()))
            .or_insert(0);
        *pass += 1;
        NodeKey {
            node: node.to_string(),
            branch,
            pass: *pass,
        }
    }

    fn push(&mut self, scope: &Scope, item: Item) {
        self.items.push((scope.clone(), item));
    }

    fn fail(&mut self, scope: &Scope, time: usize, failure: Failure) {
        self.push(scope, Item::Failed(time, Failed::Node(failure)));
    }

    /// Follows one chain until it waits, fails, ends, arrives at a join or fans out.
    fn walk(&mut self, mut chain: Chain) {
        loop {
            self.visited.insert(chain.key.node.clone());
            let Some(node) = self.definition.node(&chain.key.node) else {
                let detail = format!("node '{}' is not in the definition", chain.key.node);
                let failure =
                    Failure::new(FailureReason::NoRouteForResult, Some(chain.key), detail);
                self.fail(&chain.scope, chain.time, failure);
                return;
            };
            let key = chain.key.clone();
            let item = match self.status(&key, node) {
                Status::Done(time, typed) => {
                    self.deliver(&chain.scope, &typed);
                    match self.route(&chain, node, &typed, time) {
                        Some(next) => {
                            chain = next;
                            continue;
                        }
                        None => return,
                    }
                }
                Status::Running => Item::Running(key),
                Status::Interrupted => Item::Interrupted(key),
                Status::NeedsGate => Item::OpenGate(key),
                Status::Gated => Item::Gated(key),
                Status::Ready => Item::Ready(key),
                Status::Sleeping => Item::Sleeping(key),
                Status::Failed(time, failure) => Item::Failed(time, Failed::Node(failure)),
            };
            self.push(&chain.scope, item);
            return;
        }
    }

    fn status(&self, key: &NodeKey, node: &Node) -> Status {
        if self.state.running.contains_key(key) {
            return Status::Running;
        }
        if self.state.interrupted.contains_key(key) {
            return Status::Interrupted;
        }
        let indices = self.finishes.get(key).map(Vec::as_slice).unwrap_or(&[]);
        let Some(&last) = indices.last() else {
            return self.before_first_try(key, node);
        };
        match &self.state.finished[last].failure {
            Some(failure) => self.after_failure(key, node, indices, failure),
            None => self.after_result(key, node, indices),
        }
    }

    fn before_first_try(&self, key: &NodeKey, node: &Node) -> Status {
        let gated = node
            .gate
            .as_ref()
            .is_some_and(|gate| gate.when == GateWhen::Before);
        if !gated || self.state.closed_gates.iter().any(|gate| &gate.key == key) {
            Status::Ready
        } else if self.state.open_gates.contains_key(key) {
            Status::Gated
        } else {
            Status::NeedsGate
        }
    }

    /// A failed last try: retried while the budget lasts, else the branch fails.
    fn after_failure(
        &self,
        key: &NodeKey,
        node: &Node,
        indices: &[usize],
        failure: &Failure,
    ) -> Status {
        let floor = self.state.retry_floors.get(key).copied().unwrap_or(0);
        let failures: Vec<&Failure> = indices
            .iter()
            .filter_map(|index| self.state.finished[*index].failure.as_ref())
            .skip(floor)
            .collect();
        let retries = retries_of(self.definition, node, self.limits);
        if is_retried(failure.reason) && failures.len() <= retries {
            return Status::Ready;
        }
        let every_try_hit = failures.iter().all(|each| each.reason == failure.reason);
        let last = *indices.last().unwrap_or(&0);
        Status::Failed(
            last,
            Failure::after_retries(
                failure.reason,
                Some(key.clone()),
                failure.detail.clone(),
                every_try_hit,
            ),
        )
    }

    /// A result as last recorded: held by an after gate, revised, sleeping or done.
    fn after_result(&self, key: &NodeKey, node: &Node, indices: &[usize]) -> Status {
        let last = *indices.last().unwrap_or(&0);
        let mut value = self.state.finished[last]
            .result
            .clone()
            .unwrap_or(Value::Null);
        let gated = node
            .gate
            .as_ref()
            .is_some_and(|gate| gate.when == GateWhen::After);
        if gated {
            let successes = indices
                .iter()
                .filter(|index| self.state.finished[**index].succeeded())
                .count();
            let closes: Vec<_> = self
                .state
                .closed_gates
                .iter()
                .filter(|gate| &gate.key == key)
                .collect();
            let Some(close) = closes.last().filter(|_| closes.len() >= successes) else {
                return if self.state.open_gates.contains_key(key) {
                    Status::Gated
                } else {
                    Status::NeedsGate
                };
            };
            if close.comment.is_some() {
                return Status::Ready;
            }
            if let (Some(Value::Object(edit)), Value::Object(result)) = (&close.edit, &mut value) {
                result.extend(edit.clone());
            }
        }
        let typed = match schema::check(&node.output, &value) {
            Ok(typed) => typed,
            Err(detail) => {
                let failure =
                    Failure::new(FailureReason::NodeOutputInvalid, Some(key.clone()), detail);
                return Status::Failed(last, failure);
            }
        };
        if let Some(polls) = &node.polls
            && typed
                .fields
                .iter()
                .any(|field| field.value.as_str() == Some(polls.value.as_str()))
        {
            return Status::Sleeping;
        }
        Status::Done(last, typed)
    }

    fn deliver(&mut self, scope: &Scope, typed: &TypedResult) {
        let outcomes: Vec<Outcome> = typed
            .outcomes()
            .filter(|(_, value)| match value {
                TypedValue::List(items) => !items.is_empty(),
                value => value.as_str().is_none_or(|text| !text.trim().is_empty()),
            })
            .map(|(outcome, _)| outcome)
            .collect();
        if !outcomes.is_empty() {
            self.push(scope, Item::Delivered(outcomes));
        }
    }

    /// Routes a completed key; the next key of the same chain, if any.
    fn route(
        &mut self,
        chain: &Chain,
        node: &Node,
        typed: &TypedResult,
        time: usize,
    ) -> Option<Chain> {
        let Some(route) = choose_route(node, typed) else {
            let failure = Failure::new(
                FailureReason::NoRouteForResult,
                Some(chain.key.clone()),
                describe_unrouted(node, typed),
            );
            self.fail(&chain.scope, time, failure);
            return None;
        };
        if let Some(fan_out) = &route.fan_out {
            self.fan_out_items(chain, typed, fan_out, time);
            return None;
        }
        match &route.to {
            Some(Target::One(target)) => {
                let target = self.bounded(chain, route, target, time)?;
                let branch = chain.key.branch.clone();
                self.enter(chain.scope.clone(), branch, Some(&chain.key), &target, time)
            }
            Some(Target::Many(targets)) => {
                self.spawn_fan(chain, targets.clone(), None, time);
                None
            }
            None => {
                let failure = Failure::new(
                    FailureReason::NoRouteForResult,
                    Some(chain.key.clone()),
                    "the route has no target",
                );
                self.fail(&chain.scope, time, failure);
                None
            }
        }
    }

    /// The target of a route after its bound: itself, the `on_bound` target, or `None` when the
    /// run fails `loop bound reached`.
    fn bounded(
        &mut self,
        chain: &Chain,
        route: &Route,
        target: &str,
        time: usize,
    ) -> Option<String> {
        let Some(max) = route.max_passes else {
            return Some(target.to_string());
        };
        if self.definition.end(target).is_some() || self.next_pass(target, &chain.key.branch) <= max
        {
            return Some(target.to_string());
        }
        match route.on_bound.as_deref() {
            Some(on_bound) if on_bound != ON_BOUND_FAIL => Some(on_bound.to_string()),
            _ => {
                let detail = format!(
                    "the route from '{}' to '{target}' reached its bound of {max} passes",
                    chain.key.node
                );
                let failure = Failure::new(
                    FailureReason::LoopBoundReached,
                    Some(chain.key.clone()),
                    detail,
                );
                self.fail(&chain.scope, time, failure);
                None
            }
        }
    }

    /// Enters `target` from `from` (`None` for `start`): an end, a join arrival, or the next key.
    fn enter(
        &mut self,
        scope: Scope,
        branch: Branch,
        from: Option<&NodeKey>,
        target: &str,
        time: usize,
    ) -> Option<Chain> {
        if let Some(end) = self.definition.end(target) {
            let from = from.cloned().unwrap_or_else(|| NodeKey::new(target, 1));
            let reached = ReachedEnd {
                end: end.name.clone(),
                kind: end.kind,
                from: from.clone(),
                promised: end.outcomes.clone(),
            };
            if end.kind == EndKind::Failure {
                let failed = Failed::End {
                    end: end.name.clone(),
                    from,
                };
                self.push(&scope, Item::Failed(time, failed));
            }
            self.push(&scope, Item::End(reached));
            self.set_branch_end(&scope, BranchEnd::Ended { time });
            return None;
        }
        let joins = self
            .definition
            .node(target)
            .is_some_and(|node| node.join.is_some());
        if joins && !scope.is_empty() {
            let join = target.to_string();
            self.set_branch_end(&scope, BranchEnd::Arrived { join, time });
            return None;
        }
        Some(Chain {
            key: self.next_key(target, branch),
            scope,
            time,
        })
    }

    fn set_branch_end(&mut self, scope: &Scope, end: BranchEnd) {
        if let Some(&(fan, index)) = scope.last() {
            self.fans[fan].branches[index] = end;
        }
    }

    fn fan_out_items(&mut self, chain: &Chain, typed: &TypedResult, fan_out: &FanOut, time: usize) {
        let count = match typed.get(&fan_out.over) {
            Some(TypedValue::List(items)) => items.len(),
            _ => 0,
        };
        let width = fan_out.width.unwrap_or(self.limits.fan_out_width) as usize;
        if count > width {
            let detail = format!(
                "{count} items in '{}' are over the fan-out width of {width}",
                fan_out.over
            );
            let failure = Failure::new(
                FailureReason::BranchLimitExceeded,
                Some(chain.key.clone()),
                detail,
            );
            self.fail(&chain.scope, time, failure);
            return;
        }
        let targets = vec![fan_out.to.clone(); count];
        self.spawn_fan(chain, targets, Some(fan_out.to.clone()), time);
    }

    fn spawn_fan(
        &mut self,
        chain: &Chain,
        targets: Vec<String>,
        seed: Option<String>,
        time: usize,
    ) {
        let id = self.fans.len();
        self.fans.push(Fan {
            key: chain.key.clone(),
            scope: chain.scope.clone(),
            time,
            seed,
            branches: vec![BranchEnd::Open; targets.len()],
            resolved: false,
        });
        for (index, target) in targets.iter().enumerate() {
            let mut scope = chain.scope.clone();
            scope.push((id, index));
            let branch = self.branch_of(id, index);
            if let Some(next) = self.enter(scope, branch, Some(&chain.key), target, time) {
                self.queue.push_back(next);
            }
        }
    }

    /// The branch of a fan's branch `index`.
    fn branch_of(&self, fan: usize, index: usize) -> Branch {
        let key = &self.fans[fan].key;
        let mut branch = key.branch.clone();
        branch.push((key.node.clone(), index as u32));
        branch
    }

    /// Decides a fan-out's join once its branches allow: true when it changed something.
    fn resolve(&mut self, id: usize) -> bool {
        if self.fans[id].branches.is_empty() {
            self.fans[id].resolved = true;
            self.resolve_empty(id);
            return true;
        }
        let first = self.fans[id]
            .branches
            .iter()
            .enumerate()
            .filter_map(|(index, end)| match end {
                BranchEnd::Arrived { join, time } => Some((*time, index, join.clone())),
                _ => None,
            })
            .min();
        let all_done = !self.fans[id].branches.iter().any(BranchEnd::is_open);
        let Some((time, winner, join)) = first else {
            // No branch joins: the fan is done when every branch has ended.
            self.fans[id].resolved = all_done;
            return false;
        };
        let wait = self
            .definition
            .node(&join)
            .and_then(|node| node.join.as_ref())
            .map_or(JoinWait::All, |join| join.wait);
        let merge = match wait {
            JoinWait::Any => {
                self.cancel_losers(id, winner, time, &join);
                vec![winner]
            }
            JoinWait::All if all_done => self.arrivals_at(id, &join),
            JoinWait::All => return false,
        };
        self.fans[id].resolved = true;
        self.continue_at_join(id, &join, merge);
        true
    }

    /// A per-item fan-out over an empty list continues at the join its branches would have
    /// reached, with nothing to merge; with no join to find, the fan-out node fails.
    fn resolve_empty(&mut self, id: usize) {
        let join = self.fans[id]
            .seed
            .as_deref()
            .and_then(|seed| static_join(self.definition, seed));
        match join {
            Some(join) => self.continue_at_join(id, &join, Vec::new()),
            None => {
                let fan = &self.fans[id];
                let failure = Failure::new(
                    FailureReason::NoRouteForResult,
                    Some(fan.key.clone()),
                    "the fan-out list is empty and its branches reach no join",
                );
                let (scope, time) = (fan.scope.clone(), fan.time);
                self.fail(&scope, time, failure);
            }
        }
    }

    fn arrivals_at(&self, id: usize, join: &str) -> Vec<usize> {
        self.fans[id]
            .branches
            .iter()
            .enumerate()
            .filter(|(_, end)| matches!(end, BranchEnd::Arrived { join: at, .. } if at == join))
            .map(|(index, _)| index)
            .collect()
    }

    /// A join `any` took `winner` at `time`: every other branch is cancelled, except one that
    /// had already ended or failed by then, which keeps what it recorded.
    fn cancel_losers(&mut self, id: usize, winner: usize, time: usize, join: &str) {
        for index in 0..self.fans[id].branches.len() {
            if index == winner {
                continue;
            }
            let settled_before = match &self.fans[id].branches[index] {
                BranchEnd::Ended { time: ended } => *ended < time,
                BranchEnd::Open => self
                    .failure_time(id, index)
                    .is_some_and(|failed| failed < time),
                BranchEnd::Arrived { .. } => false,
            };
            if !settled_before {
                self.cancelled.push(((id, index), join.to_string()));
            }
        }
    }

    fn failure_time(&self, id: usize, index: usize) -> Option<usize> {
        self.items
            .iter()
            .filter(|(scope, _)| scope.contains(&(id, index)))
            .filter_map(|(_, item)| match item {
                Item::Failed(time, _) => Some(*time),
                _ => None,
            })
            .min()
    }

    /// Merges the branches not yet merged; once all are, the join's key runs in the fan's branch.
    fn continue_at_join(&mut self, id: usize, join: &str, merge: Vec<usize>) {
        let scope = self.fans[id].scope.clone();
        let mut pending = false;
        let mut time = self.fans[id].time;
        for index in merge {
            if let BranchEnd::Arrived { time: arrived, .. } = &self.fans[id].branches[index] {
                time = time.max(*arrived);
            }
            let branch = self.branch_of(id, index);
            let uses = self
                .merge_uses
                .entry((join.to_string(), branch.clone()))
                .or_insert(0);
            *uses += 1;
            let needed = *uses;
            let merged = self
                .state
                .merged
                .iter()
                .filter(|merged| merged.join == join && merged.branch == branch)
                .count();
            if merged < needed {
                self.push(&scope, Item::Merge(join.to_string(), branch));
                pending = true;
            }
        }
        if !pending {
            let branch = self.fans[id].key.branch.clone();
            let key = self.next_key(join, branch);
            self.queue.push_back(Chain { key, scope, time });
        }
    }

    fn finish(self) -> Plan {
        let cancelled: Vec<(usize, usize)> = self.cancelled.iter().map(|(pair, _)| *pair).collect();
        let mut plan = Plan {
            visited: self.visited,
            ..Plan::default()
        };
        let mut failed = Vec::new();
        for (scope, item) in self.items {
            if scope.iter().any(|pair| cancelled.contains(pair)) {
                if let Item::Running(key) = item {
                    plan.cancelling.push(key);
                }
                continue;
            }
            match item {
                Item::Ready(key) => plan.ready.push(key),
                Item::Running(key) => plan.running.push(key),
                Item::OpenGate(key) => plan.gates_to_open.push(key),
                Item::Gated(key) => plan.open_gates.push(key),
                Item::Sleeping(key) => plan.sleeping.push(key),
                Item::Interrupted(key) => plan.interrupted.push(key),
                Item::Merge(join, branch) => plan.merges.push((join, branch)),
                Item::Failed(time, failure) => failed.push((time, failure)),
                Item::End(end) => plan.ends.push(end),
                Item::Delivered(outcomes) => {
                    for outcome in outcomes {
                        if !plan.delivered.contains(&outcome) {
                            plan.delivered.push(outcome);
                        }
                    }
                }
            }
        }
        failed.sort_by_key(|(time, _)| *time);
        plan.failed = failed.into_iter().map(|(_, failure)| failure).collect();
        for ((fan, index), join) in &self.cancelled {
            let key = &self.fans[*fan].key;
            let mut branch = key.branch.clone();
            branch.push((key.node.clone(), *index as u32));
            if plan
                .cancelling
                .iter()
                .any(|key| key.branch.starts_with(&branch))
            {
                plan.cancels.push((join.clone(), branch));
            }
        }
        plan
    }
}

/// The first route whose `when` all match (no `when` always matches), else the `default` route.
fn choose_route<'n>(node: &'n Node, typed: &TypedResult) -> Option<&'n Route> {
    node.routes
        .iter()
        .find(|route| {
            !route.default
                && route.when.iter().all(|condition| {
                    typed.get(&condition.field).and_then(TypedValue::as_str)
                        == Some(condition.value.as_str())
                })
        })
        .or_else(|| node.routes.iter().find(|route| route.default))
}

/// "no route for mode = 'planned'", naming the routed fields' values.
fn describe_unrouted(node: &Node, typed: &TypedResult) -> String {
    let mut fields: Vec<&str> = Vec::new();
    for route in &node.routes {
        for condition in &route.when {
            if !fields.contains(&condition.field.as_str()) {
                fields.push(&condition.field);
            }
        }
    }
    let values: Vec<String> = fields
        .iter()
        .map(
            |field| match typed.get(field).and_then(TypedValue::as_str) {
                Some(value) => format!("{field} = '{value}'"),
                None => format!("{field} absent"),
            },
        )
        .collect();
    if values.is_empty() {
        format!("'{}' has no route for its result", node.name)
    } else {
        format!("no route for {}", values.join(", "))
    }
}

/// The first node with `join` reachable from `from`, breadth first over route targets.
fn static_join(definition: &Definition, from: &str) -> Option<String> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([from.to_string()]);
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(node) = definition.node(&name) else {
            continue;
        };
        if node.join.is_some() {
            return Some(name);
        }
        for route in &node.routes {
            match &route.to {
                Some(Target::One(target)) => queue.push_back(target.clone()),
                Some(Target::Many(targets)) => queue.extend(targets.iter().cloned()),
                None => {}
            }
            if let Some(fan_out) = &route.fan_out {
                queue.push_back(fan_out.to.clone());
            }
            if let Some(on_bound) = &route.on_bound {
                queue.push_back(on_bound.clone());
            }
        }
    }
    None
}
