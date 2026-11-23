//! What lanes tell a project repository: the run state of each project's lanes as this process
//! knows it ([`LaneRuns`]), the live source that makes a lane-owned task started for the landing
//! checks, the activity source a restore waits on, and the owned scope of a lane run that the
//! `lane-run-scope` approval hook reads.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use anyhow::Result;
use bongos_lanes_model::{
    LaneId, LaneState, LanesState, OwnerState, RunSubject, TaskId, short_run,
};
use bongos_project_repo::{ActivitySource, LiveSource, StateRead};

use crate::store::read_state;

/// The name of the live source and of the activity source.
pub const SOURCE_NAME: &str = "lanes";

#[derive(Default)]
struct Known {
    /// The lanes state of each project, by project id.
    projects: BTreeMap<String, LanesState>,
    /// The projects a restore holds new lane work back in, with the number of holds.
    holds: BTreeMap<String, usize>,
}

/// The run state of the lanes of each project, shared by the live source, the activity source
/// and the approval hook. It is read from each project's state store ([`Self::load`]), which only
/// the lanes themselves write, never from a change or its author.
#[derive(Clone, Default)]
pub struct LaneRuns(Arc<RwLock<Known>>);

impl gpui::Global for LaneRuns {}

/// The top-level task a lane run works for: the task its lane owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedScope {
    pub lane: LaneId,
    pub task: TaskId,
}

/// Whether an owner in `state` makes its task started: a lane holds it, working, stopped, halted
/// or preparing it ahead.
fn holds_task(state: OwnerState) -> bool {
    matches!(
        state,
        OwnerState::Active | OwnerState::Stopped | OwnerState::Halted | OwnerState::Ahead
    )
}

/// Whether an owner in `state` lets its lane's runs land inside the task on their own.
fn works_in_task(state: OwnerState) -> bool {
    matches!(state, OwnerState::Active | OwnerState::Ahead)
}

impl LaneRuns {
    fn read(&self) -> RwLockReadGuard<'_, Known> {
        self.0.read().unwrap_or_else(|error| error.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Known> {
        self.0.write().unwrap_or_else(|error| error.into_inner())
    }

    /// Sets the lanes state of `project_id`.
    pub fn set(&self, project_id: &str, state: LanesState) {
        self.write().projects.insert(project_id.to_string(), state);
    }

    /// Reads the lanes state of `project_id` from its state store.
    pub fn load(&self, project_id: &str, read: &StateRead) -> Result<()> {
        let state = read_state(read)?;
        self.set(project_id, state);
        Ok(())
    }

    /// Forgets `project_id`, as when it closes.
    pub fn forget(&self, project_id: &str) {
        self.write().projects.remove(project_id);
    }

    /// The lanes state of `project_id`; empty when it is not known.
    pub fn state(&self, project_id: &str) -> LanesState {
        self.read()
            .projects
            .get(project_id)
            .cloned()
            .unwrap_or_default()
    }

    /// The top-level task whose lane runs `run_id` as its job, in any project. `None` for a run
    /// no lane holds, as one started by hand.
    pub fn owned_scope(&self, run_id: &str) -> Option<TaskId> {
        let known = self.read();
        known
            .projects
            .values()
            .find_map(|state| scope_in(state, run_id))
            .map(|scope| scope.task)
    }

    /// The scope of the run `run_id` of `project_id` when a lane runs it as its job: the lane and
    /// the top-level task the lane owns, with the owner `active` or `ahead`. `None` for a run no
    /// lane holds, as one started by hand.
    pub fn owned_scope_in(&self, project_id: &str, run_id: &str) -> Option<OwnedScope> {
        let known = self.read();
        scope_in(known.projects.get(project_id)?, run_id)
    }

    /// Whether a lane owns `task_id` or runs a job on it, in any project.
    pub fn lane_holds(&self, task_id: &str) -> bool {
        self.read().projects.values().any(|state| {
            let owned = state.owners.iter().any(|(task, owner)| {
                task.0 == task_id && owner.lane.is_some() && holds_task(owner.state)
            });
            let working = state.lanes.values().any(|lane| {
                lane.job.as_ref().is_some_and(
                    |job| matches!(&job.subject, RunSubject::Task(task) if task.0 == task_id),
                )
            });
            owned || working
        })
    }

    /// The lane whose current job is on the task `task_id`, in any project.
    pub fn job_lane(&self, task_id: &str) -> Option<LaneId> {
        self.read().projects.values().find_map(|state| {
            state.lanes.values().find_map(|lane| {
                let job = lane.job.as_ref()?;
                matches!(&job.subject, RunSubject::Task(task) if task.0 == task_id)
                    .then(|| lane.id())
            })
        })
    }

    /// The lane the owner record of the top-level task `top` names, with the record's state and
    /// whether that lane still has a record in its project. `None` when no record of `top` names
    /// a lane.
    pub fn owner_lane(&self, top: &TaskId) -> Option<(LaneId, OwnerState, bool)> {
        self.read().projects.values().find_map(|state| {
            let owner = state.owners.get(top)?;
            let lane = owner.lane.clone()?;
            let exists = state.lanes.contains_key(&lane);
            Some((lane, owner.state, exists))
        })
    }

    /// Whether a restore holds new lane work back in `project_id`.
    pub fn is_held(&self, project_id: &str) -> bool {
        self.read().holds.contains_key(project_id)
    }

    fn hold(&self, project_id: &str) {
        *self
            .write()
            .holds
            .entry(project_id.to_string())
            .or_default() += 1;
    }

    fn release(&self, project_id: &str) {
        let mut known = self.write();
        if let Some(count) = known.holds.get_mut(project_id) {
            *count -= 1;
            if *count == 0 {
                known.holds.remove(project_id);
            }
        }
    }

    /// A line for each lane of `project_id` with a job in flight.
    fn working(&self, project_id: &str) -> Vec<String> {
        let known = self.read();
        let Some(state) = known.projects.get(project_id) else {
            return Vec::new();
        };
        state
            .lanes
            .values()
            .filter_map(|lane| {
                let job = lane.job.as_ref()?;
                (lane.state == LaneState::Running).then(|| {
                    format!(
                        "lane {} running {}, run {}",
                        lane.id(),
                        job.workflow,
                        short_run(&job.run_id)
                    )
                })
            })
            .collect()
    }
}

/// The lane whose job is `run_id` in `state`, and the task its owner record names in state
/// `active` or `ahead`.
fn scope_in(state: &LanesState, run_id: &str) -> Option<OwnedScope> {
    let lane = state
        .lanes
        .values()
        .find(|lane| lane.job.as_ref().is_some_and(|job| job.run_id == run_id))?
        .id();
    let task = state.owners.iter().find_map(|(task, owner)| {
        (owner.lane.as_ref() == Some(&lane) && works_in_task(owner.state)).then(|| task.clone())
    })?;
    Some(OwnedScope { lane, task })
}

/// Makes a task that a lane owns (or works on) started, so a landing never changes the shape of
/// work a lane holds.
pub struct LaneLiveSource {
    pub runs: LaneRuns,
}

impl LiveSource for LaneLiveSource {
    fn name(&self) -> &str {
        SOURCE_NAME
    }

    fn started(&self, task_id: &str) -> bool {
        self.runs.lane_holds(task_id)
    }
}

/// Reports the lanes running a job in a project and holds back the lane work that would start.
/// Lanes run in the engine host, so a stop goes there, not through this source.
pub struct LaneActivity {
    pub runs: LaneRuns,
}

impl ActivitySource for LaneActivity {
    fn name(&self) -> &str {
        SOURCE_NAME
    }

    fn activity(&self, project_id: &str) -> Vec<String> {
        self.runs.working(project_id)
    }

    fn hold_new_work(&self, project_id: &str) {
        self.runs.hold(project_id);
    }

    fn release_new_work(&self, project_id: &str) {
        self.runs.release(project_id);
    }
}
