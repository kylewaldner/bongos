//! Done owners. An owner record turns `Done` when every leaf under its task finished; it never
//! keeps its lane from other work, and while checks are still owed under the task the lane
//! lends its second tree to them ([`crate::LaneTaskTrees`]). The dispatcher clears a `Done`
//! owner at each pass (every committed state transaction starts one, the checks' writes of
//! `checks.owed` and `checks.done` among them) once no task under it owes checks or its lane no
//! longer exists. A clear is one transaction: the owner record goes and a history entry of the
//! task says why. A read that fails, a record that changed since it was seen, or a task owing
//! checks whose place the graph does not know clears nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use bongos_checks::owing_tasks_in;
use bongos_lanes_model::{LaneId, LanesState, OwnerRecord, OwnerState, PickGraph, TaskId};
use bongos_project_repo::StateTx;
use gpui::App;

use crate::dispatch_job::append_history;
use crate::dispatcher::Dispatcher;
use crate::runs::LaneRuns;
use crate::store::{delete_owner, read_state, read_state_in};
use crate::task_trees::{LaneTaskTrees, TopLevelOf};

/// The history entry of a cleared owner whose task owes no more checks.
pub const CLEARED_NO_CHECKS: &str = "lane owner cleared: no checks owed";

/// Whether checks are owed under a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owed {
    Yes,
    No,
    /// A task owes checks and the graph does not place it.
    Unknown,
}

/// Whether `top` or a task under it is among `owing`, by `graph`.
fn owed_under(graph: &PickGraph, owing: &BTreeSet<String>, top: &TaskId) -> Owed {
    let mut unknown = false;
    for task in owing {
        let id = TaskId::new(task.as_str());
        if &id == top {
            return Owed::Yes;
        }
        if !walked(graph, &id) {
            unknown = true;
        } else if &graph.top_level_of(&id) == top {
            return Owed::Yes;
        }
    }
    if unknown { Owed::Unknown } else { Owed::No }
}

/// Whether `graph` walks `id`, so knows the top-level task above it.
fn walked(graph: &PickGraph, id: &TaskId) -> bool {
    graph.order(id) != u32::MAX
}

/// The top-level lookup of the tasks `graph` walks.
pub(crate) fn top_level_from(graph: PickGraph) -> TopLevelOf {
    let graph = Arc::new(graph);
    Arc::new(move |task| {
        let id = TaskId::new(task);
        walked(&graph, &id).then(|| graph.top_level_of(&id))
    })
}

/// Clears in `tx` each `Done` owner of `seen` (as read before the transaction) that is still
/// exactly as seen, and under which no task owes checks or whose lane no longer exists
/// (`lane_exists` says whether the lane table still has it). The markers, done records and
/// owners are read in `tx`, so any read error aborts with nothing cleared. Returns the cleared
/// tasks.
pub(crate) fn clear_done_owners_in(
    tx: &mut StateTx,
    seen: &BTreeMap<TaskId, OwnerRecord>,
    graph: &PickGraph,
    lane_exists: &dyn Fn(&LaneId) -> bool,
    now: &str,
) -> Result<Vec<TaskId>> {
    let state = read_state_in(tx)?;
    let owing = owing_tasks_in(tx)?;
    let mut cleared = Vec::new();
    for (task, before) in seen {
        let Some(current) = state.owners.get(task) else {
            continue;
        };
        if current != before || current.state != OwnerState::Done {
            continue;
        }
        let gone = match &current.lane {
            Some(lane) => !state.lanes.contains_key(lane) || !lane_exists(lane),
            None => true,
        };
        let reason = if gone {
            match &current.lane {
                Some(lane) => format!("lane owner cleared: lane {lane} no longer exists"),
                None => "lane owner cleared: it names no lane".to_string(),
            }
        } else if owed_under(graph, &owing, task) == Owed::No {
            CLEARED_NO_CHECKS.to_string()
        } else {
            continue;
        };
        delete_owner(tx, task)?;
        append_history(tx, task, &reason, now)?;
        cleared.push(task.clone());
    }
    Ok(cleared)
}

/// The `Done` owners of `state`.
fn done_owners(state: &LanesState) -> BTreeMap<TaskId, OwnerRecord> {
    state
        .owners
        .iter()
        .filter(|(_, owner)| owner.state == OwnerState::Done)
        .map(|(task, owner)| (task.clone(), owner.clone()))
        .collect()
}

impl Dispatcher {
    /// The start of a pass: clears the `Done` owners that may go, then shares the lanes state
    /// with this process's [`LaneRuns`] and the graph and lanes with its [`LaneTaskTrees`] (a
    /// lane that left the table lends nothing more). Returns the lanes state the pass walks.
    pub(crate) fn begin_pass(&self, cx: &mut App) -> Result<LanesState> {
        let writer = self.parts().writer.clone();
        let mut state = writer.read(read_state)?;
        let seen = done_owners(&state);
        let trees = LaneTaskTrees::global(cx);
        let graph = if !seen.is_empty() || trees.is_some() {
            self.parts().host.graph(cx)
        } else {
            None
        };
        if let Some(graph) = &graph
            && !seen.is_empty()
        {
            let now = self.parts().host.now();
            let lane_exists = |lane: &LaneId| self.lane_exists(lane);
            match writer.transact(|tx| clear_done_owners_in(tx, &seen, graph, &lane_exists, &now)) {
                Ok(cleared) if !cleared.is_empty() => {
                    log::info!("lanes: cleared the done owners of {cleared:?}");
                    state = writer.read(read_state)?;
                }
                Ok(_) => {}
                Err(error) => log::error!("lanes: clearing done owners: {error:#}"),
            }
        }
        if let Some(runs) = cx.try_global::<LaneRuns>() {
            runs.set(&self.0.config.project, state.clone());
        }
        if let Some(trees) = trees {
            if let Some(graph) = graph {
                trees.set_top_level(top_level_from(graph));
            }
            for lane in trees.lanes() {
                if !state.lanes.contains_key(&lane) || !self.lane_exists(&lane) {
                    trees.forget_lane(&lane);
                }
            }
        }
        Ok(state)
    }
}
