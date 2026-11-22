//! A lane job's start and end. The start runs after the transaction that reserved the job's run
//! id: the job's trees and environment are prepared, then the run is started. A refusal is
//! cleared in a second transaction: one of the machine leaves the lane waiting, any other halts
//! the lane and its task. The end of a job comes from its run's status changes.

use anyhow::Result;
use bongos_lanes_model::{
    AssignedWork, Author, JobEnd, JobEndInput, LaneId, LaneRecord, LaneState, LaneTable,
    LanesState, OwnerRecord, OwnerState, PickGraph, RunSubject, TaskId, WaitReason,
    lane_after_retry, owner_after_job_end, short_run, stop_at_job_boundary,
};
use bongos_project_repo::{CommitId, StateTx};
use bongos_work_model::HistoryEntry;
use bongos_workflow::{RunStatus, StartRequest, StartSubject, WorkflowEvent};
use bongos_workflow_model::InputValue;
use bongos_workflow_model::record::{LaneRef, StartedBy};
use gpui::App;

use crate::dispatcher::{Dispatcher, JobSetup, PendingJob, environment_refusal};
use crate::job_trees::CurrentJobTrees;
use crate::store::{put_lane, put_owner, read_state, read_state_in};
use crate::task_trees::LaneTaskTrees;

/// Who the history entries of lanes are written by.
pub const HISTORY_BY: &str = "lanes";

/// How a refused start is cleared.
enum Cleared {
    Wait(WaitReason),
    Halt(String),
}

impl Dispatcher {
    /// Prepares the job's trees and environment, then starts its run.
    pub(crate) fn start_job(&self, pending: PendingJob, cx: &mut App) {
        let setup = self
            .parts()
            .host
            .prepare_job(&pending.lane, &pending.job, cx);
        let weak = self.downgrade();
        cx.spawn(async move |cx| {
            let setup = setup.await;
            if let Some(inner) = weak.upgrade() {
                cx.update(|cx| Dispatcher(inner).finish_start(pending, setup, cx));
            }
        })
        .detach();
    }

    fn finish_start(&self, pending: PendingJob, setup: Result<JobSetup, WaitReason>, cx: &mut App) {
        let cleared = match setup {
            Err(reason) => Some(Cleared::Wait(reason)),
            Ok(setup) => {
                let holds = self
                    .parts()
                    .writer
                    .read(read_state)
                    .map(|state| lane_holds(&state, &pending.lane, &pending.job.run_id));
                match holds {
                    Ok(true) => self.start_run(&pending, setup, cx),
                    Ok(false) => {
                        log::info!(
                            "lanes: {} no longer holds run {}, not starting it",
                            pending.lane,
                            pending.job.run_id
                        );
                        return;
                    }
                    // The job cannot be started, so it is cleared and the lane waits.
                    Err(error) => {
                        log::error!("lanes: reading the state before a start: {error:#}");
                        Some(Cleared::Wait(WaitReason::Other(format!(
                            "reading the state before a start: {error:#}"
                        ))))
                    }
                }
            }
        };
        let Some(cleared) = cleared else {
            return;
        };
        let now = self.parts().host.now();
        let result = self
            .parts()
            .writer
            .transact(|tx| clear_job(tx, &pending, &cleared, &now));
        if let Err(error) = result {
            log::error!(
                "lanes: clearing the refused job of {}: {error:#}",
                pending.lane
            );
        }
        self.schedule(false, cx);
    }

    /// Starts the run of a job its lane still holds; returns how to clear a refusal.
    fn start_run(&self, pending: &PendingJob, setup: JobSetup, cx: &mut App) -> Option<Cleared> {
        let trees = setup.trees.clone();
        let lane_trees = setup.lane_trees.clone();
        let request = self.start_request(pending, setup);
        match self.parts().engine.start(request, cx) {
            Ok(_) => {
                if let Some(trees) = trees {
                    CurrentJobTrees::set(pending.lane.clone(), trees, cx);
                }
                if let (Some(setup), Some(task_trees)) = (lane_trees, LaneTaskTrees::global(cx)) {
                    task_trees.set_lane(&self.0.config.project, pending.lane.clone(), setup);
                }
                None
            }
            Err(refusal) if environment_refusal(refusal.kind) => {
                Some(Cleared::Wait(WaitReason::Environment(refusal.to_string())))
            }
            Err(refusal) => Some(Cleared::Halt(refusal.to_string())),
        }
    }

    fn start_request(&self, pending: &PendingJob, setup: JobSetup) -> StartRequest {
        let config = &self.0.config;
        let subject = match &pending.work {
            AssignedWork::Leaf { task, todo_index } => StartSubject::Task {
                id: task.0.clone(),
                todo_index: *todo_index,
            },
            AssignedWork::Extension {
                task: Some(task), ..
            } => StartSubject::Task {
                id: task.0.clone(),
                todo_index: None,
            },
            AssignedWork::Extension { task: None, .. } => StartSubject::Project {
                id: config.project.clone(),
            },
        };
        let unattended = self.job_unattended(&pending.lane);
        StartRequest {
            subject,
            workflow: Some(pending.job.workflow.clone()),
            overrides: pending
                .inputs
                .iter()
                .map(|(name, value)| (name.clone(), InputValue::Text(value.clone())))
                .collect(),
            started_by: StartedBy::Lane,
            run_id: Some(pending.job.run_id.clone()),
            parent: Some(config.run_id.clone()),
            lane: Some(LaneRef {
                id: pending.lane.to_string(),
                kind: pending.lane.kind.clone(),
            }),
            lane_dirs: setup.lane_dirs,
            tree: setup.tree,
            env: setup.env,
            unattended: Some(unattended),
            definition_commit: pending.job.definition_commit.clone().map(CommitId),
            queue: false,
        }
    }

    /// A run's status changed: ends, failures and retries of lane jobs change their lanes and
    /// owners; then the lanes are evaluated, since a run's end may free an instance.
    pub fn on_workflow_event(&self, event: &WorkflowEvent, cx: &mut App) {
        let WorkflowEvent::Changed { run, state, .. } = event;
        let graph = self.parts().host.graph(cx);
        let now = self.parts().host.now();
        let table = self.table();
        let result = self
            .parts()
            .writer
            .transact(|tx| job_changed(tx, run, *state, graph.as_ref(), &table, &now));
        match result {
            Ok(Some(lane)) if matches!(state, RunStatus::Succeeded | RunStatus::Cancelled) => {
                CurrentJobTrees::clear(&lane, cx);
            }
            Ok(_) => {}
            Err(error) => log::error!("lanes: the change of run {run}: {error:#}"),
        }
        self.schedule(false, cx);
    }
}

/// Clears a job whose start was refused, when the lane still holds it.
fn clear_job(tx: &mut StateTx, pending: &PendingJob, cleared: &Cleared, now: &str) -> Result<()> {
    let state = read_state_in(tx)?;
    let Some(lane) = state.lanes.get(&pending.lane) else {
        return Ok(());
    };
    let holds = lane
        .job
        .as_ref()
        .is_some_and(|job| job.run_id == pending.job.run_id);
    if !holds {
        return Ok(());
    }
    let owner = job_owner(&state, lane);
    let mut lane = lane.clone();
    lane.job = None;
    match cleared {
        Cleared::Wait(reason) => {
            lane.state = LaneState::Waiting;
            lane.reason = Some(reason.clone());
        }
        Cleared::Halt(text) => {
            lane.state = LaneState::Halted;
            lane.reason = Some(WaitReason::Failed(text.clone()));
            if let Some((task, owner)) = &owner {
                let mut owner = owner.clone();
                owner.state = OwnerState::Halted;
                owner.reason = Some(text.clone());
                owner.by = Author::System;
                owner.since = now.to_string();
                put_owner(tx, task, &owner)?;
                append_history(tx, task, &format!("start refused: {text}"), now)?;
            }
        }
    }
    put_lane(tx, &lane)
}

/// Applies a run's status change to the lane whose job it is. Returns that lane.
fn job_changed(
    tx: &mut StateTx,
    run: &str,
    status: RunStatus,
    graph: Option<&PickGraph>,
    table: &LaneTable,
    now: &str,
) -> Result<Option<LaneId>> {
    let state = read_state_in(tx)?;
    let Some(lane) = state
        .lanes
        .values()
        .find(|lane| lane.job.as_ref().is_some_and(|job| job.run_id == run))
        .cloned()
    else {
        return Ok(None);
    };
    let id = lane.id();
    let held = job_owner(&state, &lane);
    let short = short_run(run);
    let outcome = match status {
        RunStatus::Succeeded => JobEnd::Succeeded,
        RunStatus::Failed => JobEnd::Failed(format!("run {short} failed")),
        RunStatus::Halted => JobEnd::Halted(format!("run {short} halted")),
        RunStatus::Cancelled => JobEnd::Cancelled,
        RunStatus::Queued
        | RunStatus::Running
        | RunStatus::Pausing
        | RunStatus::Paused
        | RunStatus::WaitingOnYou => {
            let live = matches!(status, RunStatus::Queued | RunStatus::Running);
            if live && lane.state == LaneState::Halted {
                match &held {
                    Some((task, owner)) => {
                        if let Some(transition) = lane_after_retry(owner, &lane, now) {
                            put_lane(tx, &transition.lane)?;
                            put_owner(tx, task, &transition.owner)?;
                            if let Some(history) = &transition.history {
                                append_history(tx, task, history, now)?;
                            }
                        }
                    }
                    // A job with no owner resumes its lane on retry; no owner is written.
                    None => {
                        let mut resumed = lane;
                        resumed.state = LaneState::Running;
                        resumed.reason = None;
                        put_lane(tx, &resumed)?;
                    }
                }
            }
            return Ok(Some(id));
        }
    };
    // An end applies once. The lane holds this run (it was found by it), so a duplicate end after
    // the job was cleared never gets here. A running lane takes any end; a lane halted by this
    // run's failure takes only a cancel, since its failure was already applied.
    let applies = match lane.state {
        LaneState::Running => true,
        LaneState::Halted => matches!(outcome, JobEnd::Cancelled),
        LaneState::Idle | LaneState::Paused | LaneState::Waiting | LaneState::Stopped => false,
    };
    if !applies {
        return Ok(None);
    }
    let lane_kind_is_ahead = table
        .kinds
        .iter()
        .any(|kind| kind.name == id.kind && kind.ahead.is_some());
    let all_leaves_finished = match (&held, graph) {
        (Some((task, _)), Some(graph)) => all_finished(graph, task),
        _ => false,
    };
    let input = JobEndInput {
        outcome,
        lane_kind_is_ahead,
        all_leaves_finished,
        checks_owed: false,
        prepared_commit: None,
        now,
    };
    let owner = held
        .as_ref()
        .map(|(_, owner)| owner.clone())
        .unwrap_or_else(|| placeholder_owner(now));
    let transition = owner_after_job_end(&owner, &lane, &input);
    let (lane_after, owner_after) = if transition.lane.state == LaneState::Idle {
        stop_at_job_boundary(&transition.lane, Some(&transition.owner), now)
    } else {
        (transition.lane.clone(), Some(transition.owner.clone()))
    };
    let mut lane_after = lane_after;
    if lane.pause_requested {
        match lane_after.state {
            // The pause is applied by the next pass; it keeps who asked for it.
            LaneState::Idle => lane_after.reason = lane.reason,
            LaneState::Paused | LaneState::Stopped => lane_after.pause_requested = false,
            LaneState::Running | LaneState::Halted | LaneState::Waiting => {}
        }
    }
    put_lane(tx, &lane_after)?;
    if let (Some((task, before)), Some(owner_after)) = (&held, owner_after) {
        if *before != owner_after {
            put_owner(tx, task, &owner_after)?;
        }
        if let Some(history) = &transition.history {
            append_history(tx, task, history, now)?;
        }
    }
    Ok(Some(id))
}

/// Whether `lane` still holds the job with `run_id`.
fn lane_holds(state: &LanesState, lane: &LaneId, run_id: &str) -> bool {
    state
        .lanes
        .get(lane)
        .and_then(|lane| lane.job.as_ref())
        .is_some_and(|job| job.run_id == run_id)
}

/// The task of a lane's job and its owner record; none for a job with no task.
fn job_owner(state: &LanesState, lane: &LaneRecord) -> Option<(TaskId, OwnerRecord)> {
    let RunSubject::Task(task) = &lane.job.as_ref()?.subject else {
        return None;
    };
    let owner = state.owners.get(task)?;
    Some((task.clone(), owner.clone()))
}

/// Whether every leaf under `task` is finished; `false` when the graph does not have it.
fn all_finished(graph: &PickGraph, task: &TaskId) -> bool {
    if graph.task(task).is_none() {
        return false;
    }
    graph
        .leaves_under(task)
        .iter()
        .all(|leaf| leaf.is_finished())
}

/// The owner a job with no task is ended against; it is never written.
fn placeholder_owner(now: &str) -> OwnerRecord {
    OwnerRecord {
        lane: None,
        state: OwnerState::Active,
        since: now.to_string(),
        by: Author::System,
        reason: None,
        prepared_commit: None,
        refreshed: false,
    }
}

pub(crate) fn append_history(
    tx: &mut StateTx,
    task: &TaskId,
    event: &str,
    now: &str,
) -> Result<()> {
    append_history_by(tx, task, event, HISTORY_BY, now)
}

/// Appends `event` to the task's history, written by `by`.
pub(crate) fn append_history_by(
    tx: &mut StateTx,
    task: &TaskId,
    event: &str,
    by: &str,
    now: &str,
) -> Result<()> {
    let at = now
        .parse()
        .map_err(|error| anyhow::anyhow!("the time {now} is not a timestamp: {error}"))?;
    tx.append_history(
        &task.0,
        HistoryEntry {
            at,
            by: by.to_string(),
            event: event.to_string(),
            detail: None,
            extra: Default::default(),
        },
    )
}
