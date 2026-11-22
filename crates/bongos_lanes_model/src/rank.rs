//! Remaining chains and rank keys over a [`PickGraph`]: "started first, then the longest
//! remaining chain, then order", applied at every level from the top-level task down to the leaf.
//! Recomputed on every call and never stored.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::graph::{PickGraph, Shape};
use crate::state::TaskId;

/// Remaining jobs and remaining chain of every walked task.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chains {
    remaining: BTreeMap<TaskId, u32>,
    chain: BTreeMap<TaskId, u32>,
}

impl Chains {
    /// `remaining(leaf) = size` (default 1) unless finished; `remaining(group)` is the sum over
    /// its children. `chain(t) = remaining(t) + max(chain(d))` over the not finished dependents
    /// `d` of `t` (siblings with an edge to `t`). A cycle of edges counts each task once.
    pub fn compute(graph: &PickGraph) -> Self {
        let mut chains = Self::default();
        for task in graph.walk() {
            chains.remaining_of(graph, &task.id);
        }
        for task in graph.walk() {
            let mut visiting = BTreeSet::new();
            let (value, _) = chains.chain_of(graph, &task.id, &mut visiting);
            chains.chain.entry(task.id.clone()).or_insert(value);
        }
        chains
    }

    pub fn remaining(&self, id: &TaskId) -> u32 {
        self.remaining.get(id).copied().unwrap_or(0)
    }

    pub fn chain(&self, id: &TaskId) -> u32 {
        self.chain.get(id).copied().unwrap_or(0)
    }

    fn remaining_of(&mut self, graph: &PickGraph, id: &TaskId) -> u32 {
        if let Some(&value) = self.remaining.get(id) {
            return value;
        }
        let Some(task) = graph.task(id) else {
            return 0;
        };
        let value = match task.shape {
            Shape::Leaf if task.is_finished() => 0,
            Shape::Leaf => task.size.unwrap_or(1),
            Shape::Group => {
                let children: Vec<TaskId> = task
                    .children
                    .iter()
                    .filter(|child| graph.parent(child) == Some(id))
                    .cloned()
                    .collect();
                children
                    .iter()
                    .map(|child| self.remaining_of(graph, child))
                    .fold(0u32, u32::saturating_add)
            }
        };
        self.remaining.insert(id.clone(), value);
        value
    }

    /// The chain of `id` and whether its computation met a cycle. Only values computed without
    /// one are memoized, since inside a cycle a value depends on where the walk entered it.
    fn chain_of(
        &mut self,
        graph: &PickGraph,
        id: &TaskId,
        visiting: &mut BTreeSet<TaskId>,
    ) -> (u32, bool) {
        if let Some(&value) = self.chain.get(id) {
            return (value, false);
        }
        if !visiting.insert(id.clone()) {
            return (0, true);
        }
        let own = self.remaining_of(graph, id);
        let mut longest = 0;
        let mut cyclic = false;
        for dependent in graph.dependents(id) {
            if graph
                .task(&dependent)
                .is_some_and(|task| task.is_finished())
            {
                continue;
            }
            let (value, hit) = self.chain_of(graph, &dependent, visiting);
            longest = longest.max(value);
            cyclic |= hit;
        }
        visiting.remove(id);
        let value = own.saturating_add(longest);
        if !cyclic {
            self.chain.insert(id.clone(), value);
        }
        (value, cyclic)
    }
}

/// One level of a rank key: lower is better.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankLevel {
    pub task: TaskId,
    pub started: bool,
    pub chain: u32,
    pub order: u32,
}

impl Ord for RankLevel {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .started
            .cmp(&self.started)
            .then(other.chain.cmp(&self.chain))
            .then(self.order.cmp(&other.order))
            .then(self.task.cmp(&other.task))
    }
}

impl PartialOrd for RankLevel {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The rank of a leaf: one [`RankLevel`] per task on its path, top-level task first, compared
/// level by level. Lower is better.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RankKey {
    pub levels: Vec<RankLevel>,
}

impl RankKey {
    /// The sentence an assignment gives: "started first; chain 11 jobs; order 4", from the
    /// top-level level and the leaf's order.
    pub fn reason(&self) -> String {
        let (Some(top), Some(leaf)) = (self.levels.first(), self.levels.last()) else {
            return String::new();
        };
        let started = if top.started {
            "started first"
        } else {
            "not started"
        };
        let jobs = if top.chain == 1 { "job" } else { "jobs" };
        format!(
            "{started}; chain {} {jobs}; order {}",
            top.chain, leaf.order
        )
    }
}

/// The rank key of `id`. `started(task, depth)` says whether the task at `depth` (0 for the
/// top-level task) counts as started.
pub fn rank_key(
    graph: &PickGraph,
    chains: &Chains,
    id: &TaskId,
    started: &dyn Fn(&TaskId, usize) -> bool,
) -> RankKey {
    RankKey {
        levels: graph
            .path(id)
            .into_iter()
            .enumerate()
            .map(|(depth, task)| RankLevel {
                started: started(&task, depth),
                chain: chains.chain(&task),
                order: graph.order(&task),
                task,
            })
            .collect(),
    }
}

/// The state-free rank key: a task counts as started when a leaf under it started or finished.
pub fn plain_rank_key(graph: &PickGraph, chains: &Chains, id: &TaskId) -> RankKey {
    rank_key(graph, chains, id, &|task, _| graph.has_started_leaf(task))
}

/// The largest remaining chain among the unfinished top-level tasks; 0 with none.
pub fn critical_path(graph: &PickGraph) -> u32 {
    let chains = Chains::compute(graph);
    graph
        .top()
        .iter()
        .filter(|id| graph.task(id).is_some_and(|task| !task.is_finished()))
        .map(|id| chains.chain(id))
        .max()
        .unwrap_or(0)
}
