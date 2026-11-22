//! The picker: which leaf a free lane works next, and the dry run over every lane.
//!
//! [`pick`] is pure and deterministic: it reads only the [`PickGraph`], the [`LanesState`], the
//! [`LaneTable`] and the [`PickOptions`] it is given and writes nothing. Its result depends on the
//! content of its inputs, never on their order. The rules, for lane L of kind K:
//!
//! 1. Stamp: with a recorded stamp ([`PickOptions::stamp`]) that differs from the graph's, every
//!    lane gets nothing and waits with [`WaitReason::StaleGraph`]. Without one the check is off.
//! 2. Own task first: a lane that owns a top-level task picks only under it.
//! 3. Free lane candidates: leaves whose owner is free for L.
//! 4. Eligibility of each candidate.
//! 5. Ahead kinds: a free lane of a kind with `ahead` prepares unowned tasks whose top-level
//!    task's edge targets are all finished, while the ahead cap has room ([`WaitReason::AheadCap`]).
//!    The assignment sets the owner `ahead`. Area rules do not apply to ahead kinds.
//! 6. Areas: see [`area_conflict`] and the "Area rule" section below.
//! 7. Rank: the best rank key wins.
//! 8. Staleness: a first job on a `prepared`, not yet refreshed owner whose rule has
//!    `staleness` carries [`Assignment::refresh_first`].
//!
//! # Area rule
//!
//! The unit the area rule reasons about is the owner of a leaf (its top-level task, or the leaf
//! itself with `owner = "self"`), with the area tags of the unit and everything under it, as
//! [`parallel`] uses. A unit is in flight when it is owned `active` (on any lane), `stopped` or
//! `halted`, or when it is started past its first job (a leaf under it finished, or two leaves
//! started); a unit whose leaves are all finished, or whose owner is `done`, is not. `ahead` and
//! `prepared` owners alone are not in flight. A unit "on another lane" is one whose owner record
//! names a lane other than L, or no lane.
//!
//! For a lane of a kind with `areas` on and no `ahead`, a candidate unit C:
//! - when C is not in flight (L would start it): waits while a conflicting unit is in flight on
//!   another lane, or has a live job on another lane;
//! - when C is in flight (L owns it, or L would resume or adopt started work): waits while a
//!   conflicting unit in flight on another lane is earlier in order, or has a live job on
//!   another lane. Earlier order wins, so two conflicting in-flight units never wait on each
//!   other.
//!
//! A live job is a holding owner record whose lane record is `running`, or a leaf with an active
//! run. Halted and stopped units stay in flight until adopted, resumed or finished.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::graph::{PickGraph, PickTask};
use crate::rank::{Chains, RankKey, RankLevel, critical_path, rank_key};
use crate::stamp::{self, StampDiff, StampFile};
use crate::state::{
    Author, LaneId, LaneState, LanesState, OwnerRecord, OwnerState, PinTarget, TaskId, WaitReason,
    short_run,
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
    /// The recorded graph stamp (`stamp.toml`); `None` turns the stamp check (rule 1) off.
    pub stamp: Option<StampFile>,
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
    /// Run the staleness script first, and the rule's refresh workflow when it says stale
    /// (rule 8).
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
    refresh_first: bool,
}

/// The workflow a dispatch rule (or the open todo) gives a leaf.
struct Matched {
    workflow: String,
    inputs: Vec<(String, String)>,
    /// The matched rule sets `staleness`.
    staleness: bool,
}

/// Where a unit is in flight, for the area rule.
#[derive(Debug, Clone)]
struct Flight {
    lane: Option<LaneId>,
    /// "in flight on build-2", "halted on build-2", "stopped", "in flight".
    where_: String,
}

/// An owner unit as the area rule sees it.
#[derive(Debug, Clone)]
struct Unit {
    order: u32,
    areas: BTreeSet<String>,
    flight: Option<Flight>,
    /// A live job: the lane it runs on (`None` for a run on no lane) and where, for messages.
    live: Option<(Option<LaneId>, String)>,
}

/// What every candidate of one lane's evaluation shares.
struct LaneContext<'a> {
    kind: Option<&'a LaneKind>,
    /// The kind runs ahead with this cap.
    ahead_cap: Option<u32>,
    /// The area rule applies.
    areas: bool,
    /// Every unit by id, empty when the area rule does not apply.
    units: BTreeMap<TaskId, Unit>,
    /// Owners prepared and not started plus owners being prepared.
    ahead_count: u32,
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
    /// How the graph differs from the recorded stamp, when it does (rule 1).
    stale: Option<StampDiff>,
}

/// Picks the work of `lane` (rules 1 to 8).
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

/// The first tag (in sorted order) two tasks share that no exemption allows, unless the two tasks
/// are an exempted pair.
///
/// Exemptions: an `[[exempt]] tasks` entry exempts every pair of distinct tasks it lists. An
/// `[[exempt]] tags` entry exempts every pair of the tags it lists, a tag with itself included,
/// so "a task tagged x and a task tagged y may run together" for any x and y in the entry.
/// Because two tasks conflict only through a tag they both carry, that means a tag listed in any
/// tags entry never conflicts: `tags = ["editor", "billing"]` lets two `editor` tasks run
/// together, as well as two `billing` tasks. A one-tag entry exempts just that tag.
pub fn area_conflict(
    table: &LaneTable,
    a: &TaskId,
    a_areas: &BTreeSet<String>,
    b: &TaskId,
    b_areas: &BTreeSet<String>,
) -> Option<String> {
    let pair_exempt = a != b
        && table.exemptions.iter().any(|exemption| {
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
        let stale = opts.stamp.as_ref().and_then(|recorded| {
            let current = stamp::compute(graph, table);
            (!current.matches(recorded)).then(|| stamp::diff(recorded, &current))
        });
        Self {
            graph,
            table,
            opts,
            chains: Chains::compute(graph),
            stale,
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
                    refresh_first: best.value.refresh_first,
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
        // Rule 1: a stale graph stops every pick; running jobs are not this function's business.
        if let Some(diff) = &self.stale {
            let task = diff
                .first_task()
                .or_else(|| self.graph.top().first())
                .cloned()
                .unwrap_or_else(|| TaskId::new(""));
            return Ok(Evaluation {
                eligible: Vec::new(),
                waiting: vec![Ranked {
                    rank: RankKey { levels: Vec::new() },
                    task,
                    value: WaitReason::StaleGraph { diff: diff.clone() },
                }],
                released: Vec::new(),
            });
        }
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
            // A task missing from the graph is not finished: hold the lane's claim and wait.
            if self.graph.task(task).is_none() {
                return Ok(Evaluation {
                    eligible: Vec::new(),
                    waiting: vec![Ranked {
                        rank: RankKey { levels: Vec::new() },
                        task: task.clone(),
                        value: WaitReason::Other("task is missing from the graph".to_string()),
                    }],
                    released: Vec::new(),
                });
            }
            let leaves = self.graph.leaves_under(task);
            // No leaves at all would be vacuously finished: wait instead of freeing the lane.
            if leaves.is_empty() {
                return Ok(Evaluation {
                    eligible: Vec::new(),
                    waiting: vec![Ranked {
                        rank: RankKey { levels: Vec::new() },
                        task: task.clone(),
                        value: WaitReason::Other("task has no leaves in the graph".to_string()),
                    }],
                    released: Vec::new(),
                });
            }
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
        // Rule 5: a lane resumes its own unfinished preparation before anything else.
        let preparing = state.owners.iter().find(|(_, owner)| {
            owner.state == OwnerState::Ahead && owner.lane.as_ref() == Some(lane)
        });
        if let Some((task, _)) = preparing {
            let leaves = self.graph.leaves_under(task);
            if leaves.iter().any(|leaf| !leaf.is_finished()) {
                let mut evaluation = self.rank_candidates(state, lane, &leaves, false);
                evaluation.released = released;
                return Ok(evaluation);
            }
        }
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
        let ctx = self.lane_context(state, lane);
        let mut eligible = Vec::new();
        let mut waiting = Vec::new();
        for leaf in leaves {
            if leaf.is_finished() {
                continue;
            }
            let rank = rank_key(self.graph, &self.chains, &leaf.id, &started);
            match self.check(&ctx, state, lane, leaf, owned) {
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
        // Rule 7: rank.
        eligible.sort_by(|a, b| a.rank.cmp(&b.rank));
        waiting.sort_by(|a, b| a.rank.cmp(&b.rank));
        Evaluation {
            eligible,
            waiting,
            released: Vec::new(),
        }
    }

    /// What every candidate of `lane` shares: its kind, whether it runs ahead, the units the area
    /// rule reads and the ahead count.
    fn lane_context(&self, state: &LanesState, lane: &LaneId) -> LaneContext<'a> {
        let kind = self.table.kinds.iter().find(|kind| kind.name == lane.kind);
        let ahead_cap = kind.and_then(|kind| kind.ahead.as_ref()).map(|ahead| ahead.cap);
        let areas = kind.is_some_and(|kind| kind.areas) && ahead_cap.is_none();
        let units = if areas {
            self.units(state)
        } else {
            BTreeMap::new()
        };
        let ahead_count = if ahead_cap.is_some() {
            self.ahead_count(state)
        } else {
            0
        };
        LaneContext {
            kind,
            ahead_cap,
            areas,
            units,
            ahead_count,
        }
    }

    /// Owners prepared with no leaf started (and still in the graph), plus owners being prepared.
    fn ahead_count(&self, state: &LanesState) -> u32 {
        state
            .owners
            .iter()
            .filter(|(task, owner)| match owner.state {
                OwnerState::Ahead => true,
                OwnerState::Prepared => {
                    self.graph.task(task).is_some() && !self.graph.has_started_leaf(task)
                }
                _ => false,
            })
            .count() as u32
    }

    /// Every owner unit of a walked leaf, with its areas, whether it is in flight and its live
    /// job.
    fn units(&self, state: &LanesState) -> BTreeMap<TaskId, Unit> {
        let keys: BTreeSet<TaskId> = self
            .graph
            .leaves()
            .map(|leaf| owner_key(self.graph, self.table, &leaf.id))
            .collect();
        keys.into_iter()
            .map(|key| {
                let unit = Unit {
                    order: self.graph.order(&key),
                    areas: self.graph.subtree_areas(&key),
                    flight: self.flight(state, &key),
                    live: self.live(state, &key),
                };
                (key, unit)
            })
            .collect()
    }

    fn flight(&self, state: &LanesState, unit: &TaskId) -> Option<Flight> {
        let leaves = self.graph.leaves_under(unit);
        if !leaves.is_empty() && leaves.iter().all(|leaf| leaf.is_finished()) {
            return None;
        }
        let at = |what: &str, lane: &Option<LaneId>| Flight {
            lane: lane.clone(),
            where_: match lane {
                Some(lane) => format!("{what} on {lane}"),
                None => what.to_string(),
            },
        };
        match state.owners.get(unit).map(|owner| (owner.state, &owner.lane)) {
            Some((OwnerState::Active, lane)) => Some(at("in flight", lane)),
            Some((OwnerState::Halted, lane)) => Some(at("halted", lane)),
            Some((OwnerState::Stopped, lane)) => Some(at("stopped", lane)),
            Some((OwnerState::Done, _)) => None,
            Some((OwnerState::Ahead | OwnerState::Prepared, _)) | None => {
                let finished = leaves.iter().any(|leaf| leaf.is_finished());
                let started = leaves.iter().filter(|leaf| leaf.started).count();
                (finished || started >= 2).then(|| at("in flight", &None))
            }
        }
    }

    fn live(&self, state: &LanesState, unit: &TaskId) -> Option<(Option<LaneId>, String)> {
        let on_running_lane = state
            .owners
            .get(unit)
            .filter(|owner| holds(owner.state))
            .and_then(|owner| owner.lane.as_ref())
            .filter(|lane| {
                state
                    .lanes
                    .get(*lane)
                    .is_some_and(|record| record.state == LaneState::Running)
            });
        if let Some(lane) = on_running_lane {
            return Some((Some(lane.clone()), format!("live job on {lane}")));
        }
        self.graph
            .leaves_under(unit)
            .iter()
            .find_map(|leaf| leaf.active_run.as_ref())
            .map(|run| (None, format!("run {} is active", short_run(run))))
    }

    /// Rule 4, with rule 3's owner check first for a free lane, rule 5 for an ahead kind, rule 6
    /// and rule 8.
    fn check(
        &self,
        ctx: &LaneContext<'_>,
        state: &LanesState,
        lane: &LaneId,
        leaf: &PickTask,
        owned: bool,
    ) -> Result<Choice, WaitReason> {
        let ahead = !owned && ctx.ahead_cap.is_some();
        let owner_change = if owned {
            None
        } else if ahead {
            Some(self.claim_ahead(state, lane, leaf)?)
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
        if ahead {
            self.check_strict(leaf)?;
        }
        if let Some(run) = &leaf.active_run {
            return Err(WaitReason::ActiveRun { run: run.clone() });
        }
        for step in path.iter().rev() {
            if let Some(reason) = self.graph.task(step).and_then(|task| task.held.clone()) {
                return Err(WaitReason::Held { reason });
            }
        }
        let Some(matched) = ctx.kind.and_then(|kind| self.workflow_for(kind, leaf)) else {
            return Err(self.no_workflow(lane, leaf));
        };
        self.check_pins(state, lane, &path)?;
        if ctx.areas {
            self.check_areas(ctx, lane, leaf, owned)?;
        }
        self.check_instances(&matched.workflow)?;
        if ahead
            && let Some(cap) = ctx.ahead_cap
        {
            let resumes = owner_change
                .as_ref()
                .is_some_and(|change| change.from == Some(OwnerState::Ahead));
            if !resumes && ctx.ahead_count >= cap {
                return Err(WaitReason::AheadCap {
                    n: ctx.ahead_count,
                    cap,
                });
            }
        }
        // Rule 8: the first job on a prepared owner that was never refreshed.
        let refresh_first = matched.staleness
            && owner_change.as_ref().is_some_and(|change| {
                change.from == Some(OwnerState::Prepared)
                    && state
                        .owners
                        .get(&change.task)
                        .is_some_and(|owner| !owner.refreshed)
            });
        Ok(Choice {
            workflow: matched.workflow,
            inputs: matched.inputs,
            owner_change,
            refresh_first,
        })
    }

    /// Rule 5's strict rule: every edge target of the leaf's top-level task is finished, not
    /// merely reached. A target missing from the graph is not finished.
    fn check_strict(&self, leaf: &PickTask) -> Result<(), WaitReason> {
        let top = self.graph.top_level_of(&leaf.id);
        let Some(task) = self.graph.task(&top) else {
            return Ok(());
        };
        let unfinished = task.edges.iter().find(|edge| {
            !self
                .graph
                .task(&edge.target)
                .is_some_and(|target| target.is_finished())
        });
        match unfinished {
            None => Ok(()),
            Some(edge) => Err(WaitReason::Dependency {
                on: edge.target.clone(),
                progress: if edge.reached {
                    format!(
                        "{} reached; an ahead lane waits until it is finished",
                        edge.unblock
                    )
                } else {
                    edge.progress.clone()
                },
            }),
        }
    }

    /// Rule 6 (see the module documentation).
    fn check_areas(
        &self,
        ctx: &LaneContext<'_>,
        lane: &LaneId,
        leaf: &PickTask,
        owned: bool,
    ) -> Result<(), WaitReason> {
        let key = owner_key(self.graph, self.table, &leaf.id);
        let Some(candidate) = ctx.units.get(&key) else {
            return Ok(());
        };
        let continuing = owned || candidate.flight.is_some();
        let mut others: Vec<(&TaskId, &Unit)> = ctx
            .units
            .iter()
            .filter(|(other, _)| **other != key)
            .collect();
        others.sort_by_key(|(other, unit)| (unit.order, *other));
        for (other, unit) in others {
            let in_flight = unit
                .flight
                .as_ref()
                .filter(|flight| flight.lane.as_ref() != Some(lane))
                .filter(|_| !continuing || unit.order < candidate.order)
                .map(|flight| flight.where_.clone());
            let live = unit
                .live
                .as_ref()
                .filter(|(on, _)| on.as_ref() != Some(lane))
                .map(|(_, where_)| where_.clone());
            let Some(where_) = in_flight.or(live) else {
                continue;
            };
            if let Some(tag) = area_conflict(self.table, &key, &candidate.areas, other, &unit.areas)
            {
                return Err(WaitReason::Area {
                    tag,
                    with: other.clone(),
                    where_,
                });
            }
        }
        Ok(())
    }

    /// Rule 5's claim: an unowned owner, or one this lane (or a lane that no longer exists) was
    /// preparing, becomes `ahead` on `lane`.
    fn claim_ahead(
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
            to: OwnerState::Ahead,
            lane: Some(lane.clone()),
        };
        let Some(owner) = state.owners.get(&key) else {
            return Ok(claim(None));
        };
        match (owner.state, &owner.lane) {
            (OwnerState::Ahead, None) => Ok(claim(Some(owner))),
            (OwnerState::Ahead, Some(other))
                if other == lane || !lane_exists(self.table, self.opts, other) =>
            {
                Ok(claim(Some(owner)))
            }
            (OwnerState::Done, _) => Err(WaitReason::Finished),
            (OwnerState::Prepared, _) => Err(WaitReason::Other("already prepared".to_string())),
            (state, Some(other)) => Err(WaitReason::Owner {
                lane: other.clone(),
                state,
            }),
            (state, None) => Err(WaitReason::Other(format!("owner is {state} with no lane"))),
        }
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
    fn workflow_for(&self, kind: &LaneKind, leaf: &PickTask) -> Option<Matched> {
        let todo_workflow = || {
            leaf.open_todo
                .as_ref()
                .and_then(|todo| todo.workflow.clone())
        };
        if kind.rules.is_empty() {
            return todo_workflow().map(|workflow| Matched {
                workflow,
                inputs: Vec::new(),
                staleness: false,
            });
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
        let workflow = match &rule.workflow {
            Some(workflow) => Some(workflow.value.clone()),
            None if rule.use_todo_workflow => todo_workflow(),
            None => None,
        }?;
        Some(Matched {
            workflow,
            inputs,
            staleness: rule.staleness.is_some(),
        })
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
