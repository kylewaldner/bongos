//! The picker: which leaf a free lane works next, and the dry run over every lane.
//!
//! [`pick`] is pure and deterministic: it reads only the [`PickGraph`], the [`LanesState`], the
//! [`LaneTable`] and the [`PickOptions`] it is given and writes nothing. Its result depends on the
//! content of its inputs, never on their order. The rules, for lane L of kind K:
//!
//! 1. Stamp (not implemented yet; see [`PickOptions`]).
//! 2. Own task first: a lane that owns a top-level task picks only under it.
//! 3. Free lane candidates: leaves whose owner is free for L.
//! 4. Eligibility of each candidate.
//! 5. Ahead kinds (not implemented yet).
//! 6. Areas (not implemented yet).
//! 7. Rank: the best rank key wins.
//! 8. Staleness (not implemented yet; [`Assignment::refresh_first`] is always false).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::graph::{PickGraph, PickTask};
use crate::rank::{Chains, RankKey, RankLevel, critical_path, rank_key};
use crate::state::{
    Author, LaneId, LaneState, LanesState, OwnerRecord, OwnerState, PinTarget, TaskId, WaitReason,
};
use crate::table::{DispatchRule, LaneKind, LaneTable, OwnerMode};

/// The use of one workflow definition's instances in the project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceUse {
    /// The definition's `instances`; `None` is unlimited.
    pub limit: Option<u32>,
    /// Active runs of the definition.
    pub active_runs: Vec<String>,
    /// Lane jobs holding a reserved run id of the definition with no run started yet.
    pub reserved: u32,
}

/// A bound the dispatcher was started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickBound {
    /// Take no new top-level task whose order is after this task's.
    Until(TaskId),
    /// Take only leaves under this group.
    Within(TaskId),
}

/// What the picker reads besides the graph, the run state and the table.
///
/// Extension points for later rules: the recorded graph stamp (rule 1), ahead caps (rule 5),
/// in-flight areas (rule 6) and staleness (rule 8) are not read yet; they become fields here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickOptions {
    /// Effective lane counts per kind (the profile's overrides); a kind not listed uses the
    /// table's `count`.
    pub counts: BTreeMap<String, u32>,
    /// Instance use per workflow definition name.
    pub instances: BTreeMap<String, InstanceUse>,
    /// Assignments made earlier in the same transaction, per workflow definition name.
    pub assigned: BTreeMap<String, u32>,
    pub bound: Option<PickBound>,
    /// How many waiting tasks the dry run lists; `None` lists all.
    pub waiting_limit: Option<usize>,
}

/// A change to an owner record that an assignment implies. The pick stays pure; the
/// dispatcher's transaction applies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerChange {
    /// The owner: a top-level task (or the leaf itself with `owner = "self"`).
    pub task: TaskId,
    /// The record's state before, `None` when there was no record.
    pub from: Option<OwnerState>,
    pub from_lane: Option<LaneId>,
    pub to: OwnerState,
    pub lane: Option<LaneId>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignedWork {
    Leaf {
        task: TaskId,
        /// The first open todo, when the leaf has one.
        todo_index: Option<usize>,
    },
    /// Work a registered extension returns; the picker never produces it.
    Extension {
        kind: String,
        task: Option<TaskId>,
        payload: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assignment {
    pub work: AssignedWork,
    pub workflow: String,
    pub inputs: Vec<(String, String)>,
    pub rank: RankKey,
    /// "started first; chain 11 jobs; order 4".
    pub reason: String,
    pub owner_change: Vec<OwnerChange>,
    /// Run the rule's refresh workflow first (rule 8; always false until it exists).
    pub refresh_first: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Waiting {
    pub task: TaskId,
    pub reason: WaitReason,
}

/// The lane's own task is halted, so the lane holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneHold {
    pub task: TaskId,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PickOutcome {
    Assign(Assignment),
    /// Nothing to do, with the reason of every candidate, best ranked first. `owner_change`
    /// carries the lane's own task becoming done when all its leaves finished.
    Nothing {
        reasons: Vec<Waiting>,
        owner_change: Vec<OwnerChange>,
    },
    Hold(LaneHold),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryRun {
    pub per_lane: Vec<(LaneId, PickOutcome)>,
    /// Waiting leaves, best ranked first, up to [`PickOptions::waiting_limit`].
    pub waiting: Vec<Waiting>,
    /// Leaves eligible for at least one lane.
    pub ready: usize,
    pub critical_path: u32,
    pub parallel: u32,
}

/// What an eligible leaf would run.
#[derive(Debug, Clone)]
struct Choice {
    workflow: String,
    inputs: Vec<(String, String)>,
    owner_change: Option<OwnerChange>,
}

struct Ranked<T> {
    rank: RankKey,
    task: TaskId,
    value: T,
}

struct Evaluation {
    eligible: Vec<Ranked<Choice>>,
    waiting: Vec<Ranked<WaitReason>>,
    /// The lane's own task, done now that its leaves all finished.
    released: Vec<OwnerChange>,
}

struct Picker<'a> {
    graph: &'a PickGraph,
    table: &'a LaneTable,
    opts: &'a PickOptions,
    chains: Chains,
}

/// Picks the work of `lane` (D5 rules 2, 3, 4 and 7).
pub fn pick(
    graph: &PickGraph,
    state: &LanesState,
    table: &LaneTable,
    lane: &LaneId,
    opts: &PickOptions,
) -> PickOutcome {
    let picker = Picker::new(graph, table, opts);
    picker.pick(state, lane)
}

/// The dry run: [`pick`] for every lane in index order against a scratch copy of the state, so
/// a later lane sees an earlier lane's hypothetical claim. Writes nothing.
pub fn explain(
    graph: &PickGraph,
    state: &LanesState,
    table: &LaneTable,
    opts: &PickOptions,
) -> DryRun {
    let lanes = table_lanes(table, opts);
    let picker = Picker::new(graph, table, opts);

    let mut ready = BTreeSet::new();
    for lane in &lanes {
        if let Ok(evaluation) = picker.evaluate(state, lane) {
            ready.extend(evaluation.eligible.into_iter().map(|ranked| ranked.task));
        }
    }

    let mut scratch = state.clone();
    let mut scratch_opts = opts.clone();
    let mut per_lane = Vec::new();
    let mut assigned = BTreeSet::new();
    for lane in &lanes {
        let outcome = Picker::new(graph, table, &scratch_opts).pick(&scratch, lane);
        match &outcome {
            PickOutcome::Assign(assignment) => {
                apply_changes(&mut scratch, &assignment.owner_change, lane);
                *scratch_opts
                    .assigned
                    .entry(assignment.workflow.clone())
                    .or_insert(0) += 1;
                if let AssignedWork::Leaf { task, .. } = &assignment.work {
                    assigned.insert(task.clone());
                }
            }
            PickOutcome::Nothing { owner_change, .. } => {
                apply_changes(&mut scratch, owner_change, lane);
            }
            PickOutcome::Hold(_) => {}
        }
        per_lane.push((lane.clone(), outcome));
    }

    let after = Picker::new(graph, table, &scratch_opts);
    let mut still_ready = BTreeSet::new();
    let mut waiting: BTreeMap<TaskId, Ranked<WaitReason>> = BTreeMap::new();
    for lane in &lanes {
        let Ok(evaluation) = after.evaluate(&scratch, lane) else {
            continue;
        };
        still_ready.extend(evaluation.eligible.into_iter().map(|ranked| ranked.task));
        for ranked in evaluation.waiting {
            waiting.entry(ranked.task.clone()).or_insert(ranked);
        }
    }
    let mut waiting: Vec<Ranked<WaitReason>> = waiting
        .into_values()
        .filter(|ranked| !assigned.contains(&ranked.task) && !still_ready.contains(&ranked.task))
        .collect();
    waiting.sort_by(|a, b| a.rank.cmp(&b.rank));
    if let Some(limit) = opts.waiting_limit {
        waiting.truncate(limit);
    }

    let live = lanes
        .iter()
        .filter(|lane| {
            state.lanes.get(*lane).is_none_or(|record| {
                !matches!(
                    record.state,
                    LaneState::Paused | LaneState::Stopped | LaneState::Halted
                )
            })
        })
        .count() as u32;
    let started = |task: &TaskId| top_started(graph, state, task);
    DryRun {
        per_lane,
        waiting: waiting
            .into_iter()
            .map(|ranked| Waiting {
                task: ranked.task,
                reason: ranked.value,
            })
            .collect(),
        ready: ready.len(),
        critical_path: critical_path(graph),
        parallel: parallel_with(graph, table, Some(live), &started),
    }
}

/// How many tasks could be worked at once: a greedy count over the ready set in rank order of
/// owner tasks pairwise free of area conflicts, capped by `cap`. Reads no run state: "started"
/// means a task with a started leaf, and nothing is in flight.
pub fn parallel(graph: &PickGraph, table: &LaneTable, cap: Option<u32>) -> u32 {
    let started = |task: &TaskId| graph.has_started_leaf(task);
    parallel_with(graph, table, cap, &started)
}

fn parallel_with(
    graph: &PickGraph,
    table: &LaneTable,
    cap: Option<u32>,
    started: &dyn Fn(&TaskId) -> bool,
) -> u32 {
    let chains = Chains::compute(graph);
    let owners: BTreeSet<TaskId> = graph
        .leaves()
        .filter(|leaf| plainly_ready(graph, leaf))
        .map(|leaf| owner_key(graph, table, &leaf.id))
        .collect();
    let mut owners: Vec<RankLevel> = owners
        .into_iter()
        .map(|task| RankLevel {
            started: started(&task),
            chain: chains.chain(&task),
            order: graph.order(&task),
            task,
        })
        .collect();
    owners.sort();
    let cap = cap.unwrap_or(u32::MAX);
    let mut taken: Vec<(TaskId, BTreeSet<String>)> = Vec::new();
    for owner in owners {
        if taken.len() as u32 >= cap {
            break;
        }
        let areas = graph.subtree_areas(&owner.task);
        let free = taken.iter().all(|(other, other_areas)| {
            area_conflict(table, &owner.task, &areas, other, other_areas).is_none()
        });
        if free {
            taken.push((owner.task, areas));
        }
    }
    taken.len() as u32
}

/// A leaf that is not finished, has every edge on its path reached, no active run and no hold.
fn plainly_ready(graph: &PickGraph, leaf: &PickTask) -> bool {
    !leaf.is_finished()
        && leaf.active_run.is_none()
        && graph.path(&leaf.id).iter().all(|step| {
            graph
                .task(step)
                .is_some_and(|task| task.held.is_none() && task.edges.iter().all(|e| e.reached))
        })
}

/// The first tag two tasks share that no exemption allows, unless the two tasks are an
/// exempted pair. A tag listed in any `[[exempt]] tags` entry never conflicts.
pub fn area_conflict(
    table: &LaneTable,
    a: &TaskId,
    a_areas: &BTreeSet<String>,
    b: &TaskId,
    b_areas: &BTreeSet<String>,
) -> Option<String> {
    let pair_exempt = table.exemptions.iter().any(|exemption| {
        let listed = |id: &TaskId| exemption.tasks.iter().any(|task| task.value == id.0);
        listed(a) && listed(b)
    });
    if pair_exempt {
        return None;
    }
    a_areas
        .intersection(b_areas)
        .find(|tag| {
            !table
                .exemptions
                .iter()
                .any(|exemption| exemption.tags.iter().any(|t| &t.value == *tag))
        })
        .cloned()
}

/// Every lane the table declares with the effective counts, sorted by kind and index.
pub fn table_lanes(table: &LaneTable, opts: &PickOptions) -> Vec<LaneId> {
    let lanes: BTreeSet<LaneId> = table
        .kinds
        .iter()
        .flat_map(|kind| {
            let count = effective_count(table, opts, &kind.name).unwrap_or(0);
            (1..=count).map(|index| LaneId::new(kind.name.clone(), index))
        })
        .collect();
    lanes.into_iter().collect()
}

fn effective_count(table: &LaneTable, opts: &PickOptions, kind: &str) -> Option<u32> {
    if !table.kinds.iter().any(|declared| declared.name == kind) {
        return None;
    }
    opts.counts.get(kind).copied().or_else(|| {
        table
            .kinds
            .iter()
            .filter(|declared| declared.name == kind)
            .map(|declared| declared.count)
            .max()
    })
}

/// Whether the lane still exists: its kind is declared and its index is within the count.
fn lane_exists(table: &LaneTable, opts: &PickOptions, lane: &LaneId) -> bool {
    effective_count(table, opts, &lane.kind).is_some_and(|count| lane.index <= count)
}

/// The owner of a leaf: its top-level task, or itself with `owner = "self"`.
fn owner_key(graph: &PickGraph, table: &LaneTable, leaf: &TaskId) -> TaskId {
    match table.owner {
        OwnerMode::Top => graph.top_level_of(leaf),
        OwnerMode::SelfOwned => leaf.clone(),
    }
}

fn holds(state: OwnerState) -> bool {
    matches!(
        state,
        OwnerState::Active | OwnerState::Stopped | OwnerState::Halted
    )
}

/// A top-level task is started when an owner record on it (or on a leaf under it) is active,
/// stopped or halted, or when a leaf under it started or finished.
fn top_started(graph: &PickGraph, state: &LanesState, task: &TaskId) -> bool {
    let owned = |id: &TaskId| state.owners.get(id).is_some_and(|owner| holds(owner.state));
    owned(task)
        || graph.has_started_leaf(task)
        || graph.leaves_under(task).iter().any(|leaf| owned(&leaf.id))
}

fn apply_changes(state: &mut LanesState, changes: &[OwnerChange], by: &LaneId) {
    for change in changes {
        let record = state
            .owners
            .entry(change.task.clone())
            .or_insert_with(|| OwnerRecord {
                lane: None,
                state: change.to,
                since: String::new(),
                by: Author::Lane(by.clone()),
                reason: None,
                prepared_commit: None,
                refreshed: false,
            });
        record.state = change.to;
        record.lane = change.lane.clone();
        record.by = Author::Lane(by.clone());
    }
}

impl<'a> Picker<'a> {
    fn new(graph: &'a PickGraph, table: &'a LaneTable, opts: &'a PickOptions) -> Self {
        Self {
            graph,
            table,
            opts,
            chains: Chains::compute(graph),
        }
    }

    fn pick(&self, state: &LanesState, lane: &LaneId) -> PickOutcome {
        let evaluation = match self.evaluate(state, lane) {
            Ok(evaluation) => evaluation,
            Err(hold) => return PickOutcome::Hold(hold),
        };
        let Evaluation {
            eligible,
            waiting,
            released,
        } = evaluation;
        match eligible.into_iter().next() {
            Some(best) => {
                let leaf = self.graph.task(&best.task);
                let mut owner_change = released;
                owner_change.extend(best.value.owner_change);
                PickOutcome::Assign(Assignment {
                    work: AssignedWork::Leaf {
                        todo_index: leaf.and_then(|leaf| leaf.open_todo.as_ref().map(|t| t.index)),
                        task: best.task,
                    },
                    workflow: best.value.workflow,
                    inputs: best.value.inputs,
                    reason: best.rank.reason(),
                    rank: best.rank,
                    owner_change,
                    refresh_first: false,
                })
            }
            None => PickOutcome::Nothing {
                reasons: waiting
                    .into_iter()
                    .map(|ranked| Waiting {
                        task: ranked.task,
                        reason: ranked.value,
                    })
                    .collect(),
                owner_change: released,
            },
        }
    }

    /// Every candidate of `lane`, eligible or waiting, each list best ranked first.
    fn evaluate(&self, state: &LanesState, lane: &LaneId) -> Result<Evaluation, LaneHold> {
        // Rule 1 (stamp) goes here.
        let mut released = Vec::new();
        let mut view: Option<LanesState> = None;
        // Rule 2: own task first.
        if let Some(task) = state.task_of(lane) {
            let owner = &state.owners[task];
            if owner.state == OwnerState::Halted {
                return Err(LaneHold {
                    task: task.clone(),
                    reason: owner.reason.clone().unwrap_or_else(|| "halted".to_string()),
                });
            }
            let leaves = self.graph.leaves_under(task);
            if leaves.iter().all(|leaf| leaf.is_finished()) {
                let change = OwnerChange {
                    task: task.clone(),
                    from: Some(owner.state),
                    from_lane: owner.lane.clone(),
                    to: OwnerState::Done,
                    lane: owner.lane.clone(),
                };
                let mut next = state.clone();
                apply_changes(&mut next, std::slice::from_ref(&change), lane);
                released.push(change);
                view = Some(next);
            } else {
                let resume = (owner.state == OwnerState::Stopped).then(|| OwnerChange {
                    task: task.clone(),
                    from: Some(OwnerState::Stopped),
                    from_lane: owner.lane.clone(),
                    to: OwnerState::Active,
                    lane: Some(lane.clone()),
                });
                let mut evaluation = self.rank_candidates(state, lane, &leaves, true);
                for ranked in &mut evaluation.eligible {
                    ranked.value.owner_change = resume.clone();
                }
                return Ok(evaluation);
            }
        }
        // Rule 3: a free lane considers every leaf.
        let state = view.as_ref().unwrap_or(state);
        let leaves: Vec<&PickTask> = self.graph.leaves().collect();
        let mut evaluation = self.rank_candidates(state, lane, &leaves, false);
        evaluation.released = released;
        Ok(evaluation)
    }

    fn rank_candidates(
        &self,
        state: &LanesState,
        lane: &LaneId,
        leaves: &[&PickTask],
        owned: bool,
    ) -> Evaluation {
        let started = |task: &TaskId, depth: usize| {
            if depth == 0 {
                top_started(self.graph, state, task)
            } else {
                self.graph.has_started_leaf(task)
            }
        };
        let mut eligible = Vec::new();
        let mut waiting = Vec::new();
        for leaf in leaves {
            if leaf.is_finished() {
                continue;
            }
            let rank = rank_key(self.graph, &self.chains, &leaf.id, &started);
            match self.check(state, lane, leaf, owned) {
                Ok(choice) => eligible.push(Ranked {
                    rank,
                    task: leaf.id.clone(),
                    value: choice,
                }),
                Err(reason) => waiting.push(Ranked {
                    rank,
                    task: leaf.id.clone(),
                    value: reason,
                }),
            }
        }
        // Rule 6 (areas) and rule 5 (ahead kinds) filter `eligible` here.
        // Rule 7: rank.
        eligible.sort_by(|a, b| a.rank.cmp(&b.rank));
        waiting.sort_by(|a, b| a.rank.cmp(&b.rank));
        Evaluation {
            eligible,
            waiting,
            released: Vec::new(),
        }
    }

    /// Rule 4, with rule 3's owner check first for a free lane.
    fn check(
        &self,
        state: &LanesState,
        lane: &LaneId,
        leaf: &PickTask,
        owned: bool,
    ) -> Result<Choice, WaitReason> {
        let owner_change = if owned {
            None
        } else {
            Some(self.claim(state, lane, leaf)?)
        };
        self.check_bound(leaf, owned)?;
        let path = self.graph.path(&leaf.id);
        for step in path.iter().rev() {
            let Some(task) = self.graph.task(step) else {
                continue;
            };
            if let Some(edge) = task.edges.iter().find(|edge| !edge.reached) {
                return Err(WaitReason::Dependency {
                    on: edge.target.clone(),
                    progress: edge.progress.clone(),
                });
            }
        }
        if let Some(run) = &leaf.active_run {
            return Err(WaitReason::ActiveRun { run: run.clone() });
        }
        for step in path.iter().rev() {
            if let Some(reason) = self.graph.task(step).and_then(|task| task.held.clone()) {
                return Err(WaitReason::Held { reason });
            }
        }
        let kind = self.table.kinds.iter().find(|kind| kind.name == lane.kind);
        let Some((workflow, inputs)) = kind.and_then(|kind| self.workflow_for(kind, leaf)) else {
            return Err(self.no_workflow(lane, leaf));
        };
        self.check_pins(state, lane, &path)?;
        self.check_instances(&workflow)?;
        Ok(Choice {
            workflow,
            inputs,
            owner_change,
        })
    }

    /// Rule 3: whether the leaf's owner is free for `lane`, and the claim that takes it.
    fn claim(
        &self,
        state: &LanesState,
        lane: &LaneId,
        leaf: &PickTask,
    ) -> Result<OwnerChange, WaitReason> {
        let key = owner_key(self.graph, self.table, &leaf.id);
        let claim = |from: Option<&OwnerRecord>| OwnerChange {
            task: key.clone(),
            from: from.map(|owner| owner.state),
            from_lane: from.and_then(|owner| owner.lane.clone()),
            to: OwnerState::Active,
            lane: Some(lane.clone()),
        };
        let Some(owner) = state.owners.get(&key) else {
            return Ok(claim(None));
        };
        match (owner.state, &owner.lane) {
            (OwnerState::Prepared, _) => Ok(claim(Some(owner))),
            (OwnerState::Stopped, None) => Ok(claim(Some(owner))),
            (OwnerState::Stopped, Some(other))
                if other == lane || !lane_exists(self.table, self.opts, other) =>
            {
                Ok(claim(Some(owner)))
            }
            (OwnerState::Done, _) => Err(WaitReason::Finished),
            (state, Some(other)) => Err(WaitReason::Owner {
                lane: other.clone(),
                state,
            }),
            (state, None) => Err(WaitReason::Other(format!("owner is {state} with no lane"))),
        }
    }

    fn check_bound(&self, leaf: &PickTask, owned: bool) -> Result<(), WaitReason> {
        match &self.opts.bound {
            None => Ok(()),
            Some(PickBound::Within(group)) => {
                if self.graph.path(&leaf.id).contains(group) {
                    Ok(())
                } else {
                    Err(WaitReason::Bound)
                }
            }
            Some(PickBound::Until(last)) => {
                if owned || self.graph.task(last).is_none() {
                    return Ok(());
                }
                let top = self.graph.top_level_of(&leaf.id);
                let last_top = self.graph.top_level_of(last);
                if self.graph.order(&top) > self.graph.order(&last_top) {
                    Err(WaitReason::Bound)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// The first matching rule of the kind's list gives the workflow; with an empty list, or a
    /// matching rule that says `use_todo_workflow`, the open todo's workflow does.
    fn workflow_for(
        &self,
        kind: &LaneKind,
        leaf: &PickTask,
    ) -> Option<(String, Vec<(String, String)>)> {
        let todo_workflow = || {
            leaf.open_todo
                .as_ref()
                .and_then(|todo| todo.workflow.clone())
        };
        if kind.rules.is_empty() {
            return todo_workflow().map(|workflow| (workflow, Vec::new()));
        }
        let rule = kind
            .rules
            .iter()
            .filter_map(|name| self.table.rules.iter().find(|rule| rule.name == name.value))
            .find(|rule| rule_matches(rule, leaf))?;
        let inputs = rule
            .inputs
            .iter()
            .map(|(name, value)| (name.clone(), value.value.clone()))
            .collect();
        match &rule.workflow {
            Some(workflow) => Some((workflow.value.clone(), inputs)),
            None if rule.use_todo_workflow => todo_workflow().map(|workflow| (workflow, inputs)),
            None => None,
        }
    }

    /// `LaneKind` when another kind's rules would give the leaf a workflow, else `NoWorkflow`.
    fn no_workflow(&self, lane: &LaneId, leaf: &PickTask) -> WaitReason {
        let mut others: Vec<&LaneKind> = self
            .table
            .kinds
            .iter()
            .filter(|kind| kind.name != lane.kind)
            .collect();
        others.sort_by(|a, b| a.name.cmp(&b.name));
        others
            .into_iter()
            .find(|kind| self.workflow_for(kind, leaf).is_some())
            .map_or(WaitReason::NoWorkflow, |kind| WaitReason::LaneKind {
                kind: kind.name.clone(),
            })
    }

    /// No pin on the leaf or an ancestor sends it to another lane or kind; a pin to a lane that
    /// is not running waits.
    fn check_pins(
        &self,
        state: &LanesState,
        lane: &LaneId,
        path: &[TaskId],
    ) -> Result<(), WaitReason> {
        let mut pins: Vec<(String, PinTarget)> = self
            .table
            .pins
            .iter()
            .filter_map(|pin| {
                let task = &pin.task.as_ref()?.value;
                path.iter()
                    .any(|step| &step.0 == task)
                    .then(|| (task.clone(), PinTarget::from(pin)))
            })
            .collect();
        pins.sort_by(|a, b| (&a.0, &a.1.lane, &a.1.kind).cmp(&(&b.0, &b.1.lane, &b.1.kind)));
        for (_, target) in pins {
            if target.allows(lane) {
                continue;
            }
            if let Some(to) = &target.lane {
                let running = LaneId::parse(to).is_some_and(|pinned| {
                    lane_exists(self.table, self.opts, &pinned)
                        && state.lanes.get(&pinned).is_none_or(|record| {
                            !matches!(
                                record.state,
                                LaneState::Paused | LaneState::Stopped | LaneState::Halted
                            )
                        })
                });
                return Err(if running {
                    WaitReason::Pin { to: to.clone() }
                } else {
                    WaitReason::PinNotRunning { to: to.clone() }
                });
            }
            return Err(WaitReason::Pin {
                to: target.kind.unwrap_or_default(),
            });
        }
        Ok(())
    }

    fn check_instances(&self, workflow: &str) -> Result<(), WaitReason> {
        let Some(usage) = self.opts.instances.get(workflow) else {
            return Ok(());
        };
        let Some(limit) = usage.limit else {
            return Ok(());
        };
        let active = usage.active_runs.len() as u32
            + usage.reserved
            + self.opts.assigned.get(workflow).copied().unwrap_or(0);
        if active >= limit {
            return Err(WaitReason::InstanceLimit {
                workflow: workflow.to_string(),
                active,
                limit,
                run: usage.active_runs.first().cloned(),
            });
        }
        Ok(())
    }
}

/// A rule matches on the work kind, the derived state and the open todo's name, each only when
/// the rule sets it.
fn rule_matches(rule: &DispatchRule, leaf: &PickTask) -> bool {
    rule.work_kind
        .as_ref()
        .is_none_or(|kind| leaf.work_kind.as_ref() == Some(kind))
        && (rule.state.is_empty() || rule.state.iter().any(|state| state == leaf.state.label()))
        && rule.todo.as_ref().is_none_or(|todo| {
            leaf.open_todo
                .as_ref()
                .is_some_and(|open| &open.name == todo)
        })
}
