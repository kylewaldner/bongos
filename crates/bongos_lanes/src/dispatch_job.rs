//! A lane job's start and end. The start runs after the transaction that reserved the job's run
//! id: the job's trees and environment are prepared, the job hooks' `before_job` are awaited (when
//! any hook is registered), then the run is started. A refusal is cleared in a second
//! transaction: one of the machine leaves the lane waiting, any other halts the lane and its
//! task. The end of a job comes from its run's status changes.
//!
//! A job that went through the job hooks is kept in [`Dispatcher`]'s `hooked` list until every
//! hook owed an `after_job` got it; its lane is not evaluated meanwhile, and the dispatcher's run
//! does not end. A pass, or the hooks' answer, drops a job whose run has not started when its lane
//! no longer holds it, was asked to stop or left the lane count: the pending `before_job` future
//! is dropped, `after_job` hears [`HookEnd::StoppedBeforeStart`], and then the job is cleared. A
//! started job is settled as soon as a status change of its run leaves its lane without it.
//!
//! A job a pick extension assigned (its work is `AssignedWork::Extension`) is settled by the pick
//! extensions first: every path that clears it before its run started tells them
//! [`JobChange::NotStarted`], a refused start [`JobChange::Refused`], and a status change of its
//! run [`JobChange::Lifecycle`], each in the transaction that records the change. Only when no
//! extension answers does the default handling below apply.

use std::sync::Arc;

use anyhow::Result;
use bongos_lanes_model::{
    AssignedWork, Author, JobEnd, JobEndInput, JobRecord, LaneId, LaneRecord, LaneState, LaneTable,
    LanesState, OwnerRecord, OwnerState, PickGraph, RunSubject, TaskId, Transition, WaitReason,
    lane_after_retry, owner_after_job_end, short_run, stop_at_job_boundary,
};
use bongos_project_repo::{CommitId, StateTx};
use bongos_work_model::HistoryEntry;
use bongos_workflow::{RunStatus, StartRequest, StartSubject, WorkflowEvent};
use bongos_workflow_model::InputValue;
use bongos_workflow_model::record::{EndState, LaneRef, Lifecycle, StartedBy};
use gpui::{App, Task};

use crate::dispatcher::{
    AheadLanding, Dispatcher, JobSetup, PendingJob, RefreshFirst, environment_refusal, held_task,
};
use crate::hooks::{BeforeJob, JobEnd as HookEnd, JobHookCx, JobHooks, after_job_of};
use crate::job_trees::CurrentJobTrees;
use crate::pick_extension::{
    JobChange, PickExtension, PickExtensions, extension_job_changed, lane_after_run,
    lane_after_unstarted, with_engine,
};
use crate::staleness::{Staleness, StalenessCheck};
use crate::store::{put_lane, put_owner, read_state, read_state_in};
use crate::task_trees::LaneTaskTrees;

/// Who the history entries of lanes are written by.
pub const HISTORY_BY: &str = "lanes";

/// How a refused start is cleared.
enum Cleared {
    Wait(WaitReason),
    Halt(String),
}

/// Whether a cleared job reached the run engine, which refused it, or never did.
#[derive(Clone, Copy)]
enum ClearedAt {
    /// Before the run engine: trees not prepared, a hook refused, the state unreadable.
    NotStarted,
    /// The run engine refused the start.
    Refused,
}

/// The reason an extension hears for a job dropped before its run started.
pub const DROPPED_BEFORE_START: &str = "dropped before its run started";

/// What a lane halts with when its job's staleness script fails, before the failure.
pub const STALENESS_FAILED: &str = "staleness script failed: ";

impl Cleared {
    fn text(&self) -> String {
        match self {
            Cleared::Wait(reason) => reason.to_string(),
            Cleared::Halt(text) => text.clone(),
        }
    }
}

/// A job that went through the job hooks, until its `after_job` resolved.
pub(crate) struct HookedJob {
    job: JobRecord,
    hcx: JobHookCx,
    phase: HookPhase,
}

enum HookPhase {
    /// `before_job` is pending; dropping the task drops the pending hook's future.
    Before(Task<()>),
    /// `before_job` resolved and the start is being decided.
    Deciding,
    /// The run started; the last state its status changes reported.
    Started(Lifecycle),
    /// `after_job` is running.
    After,
}

/// What clears a job once its hooks' `after_job` resolved.
type Then = Box<dyn FnOnce(&Dispatcher, &mut App)>;

impl Dispatcher {
    /// Prepares the job's trees and environment, runs the staleness script of a prepared task's
    /// first job, then starts its run.
    pub(crate) fn start_job(&self, pending: PendingJob, cx: &mut App) {
        let host = self.parts().host.clone();
        let setup = host.prepare_job(&pending.lane, &pending.job, cx);
        let weak = self.downgrade();
        cx.spawn(async move |cx| {
            let setup = setup.await;
            let staleness = match (&setup, &pending.refresh) {
                (Ok(setup), Some(refresh)) => {
                    let check = staleness_check(&pending, refresh, setup);
                    let answer = cx.update(|cx| host.staleness(check, cx));
                    Some(answer.await)
                }
                _ => None,
            };
            if let Some(inner) = weak.upgrade() {
                cx.update(|cx| Dispatcher(inner).finish_start(pending, setup, staleness, cx));
            }
        })
        .detach();
    }

    fn finish_start(
        &self,
        mut pending: PendingJob,
        setup: Result<JobSetup, WaitReason>,
        staleness: Option<Result<Staleness, String>>,
        cx: &mut App,
    ) {
        if setup.is_ok()
            && let Some(answer) = staleness
        {
            match self.apply_staleness(&mut pending, answer) {
                Ok(true) => {}
                Ok(false) => return,
                Err(cleared) => {
                    self.clear_start(&pending, &cleared, ClearedAt::NotStarted, cx);
                    return;
                }
            }
        }
        let cleared = match setup {
            Err(reason) => Some((Cleared::Wait(reason), ClearedAt::NotStarted)),
            Ok(setup) => {
                let holds = self
                    .parts()
                    .writer
                    .read(read_state)
                    .map(|state| lane_holds(&state, &pending.lane, &pending.job.run_id));
                match holds {
                    Ok(true) if !JobHooks::hooks(cx).is_empty() => {
                        self.begin_hooks(pending, setup, cx);
                        return;
                    }
                    Ok(true) => self
                        .start_run(&pending, setup, cx)
                        .map(|cleared| (cleared, ClearedAt::Refused)),
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
                        Some((
                            Cleared::Wait(WaitReason::Other(format!(
                                "reading the state before a start: {error:#}"
                            ))),
                            ClearedAt::NotStarted,
                        ))
                    }
                }
            }
        };
        if let Some((cleared, at)) = cleared {
            self.clear_start(&pending, &cleared, at, cx);
        }
    }

    /// Applies a staleness script's answer to a job its lane still holds: the owner is marked
    /// refreshed, and on `stale` the job runs the rule's refresh workflow instead. Returns
    /// whether the job is still to start, or how to clear it when the script failed.
    fn apply_staleness(
        &self,
        pending: &mut PendingJob,
        answer: Result<Staleness, String>,
    ) -> Result<bool, Cleared> {
        let Some(refresh) = pending.refresh.clone() else {
            return Ok(true);
        };
        let staleness =
            answer.map_err(|text| Cleared::Halt(format!("{STALENESS_FAILED}{text}")))?;
        let now = self.parts().host.now();
        let result = self
            .parts()
            .writer
            .transact(|tx| record_staleness(tx, pending, &refresh, staleness, &now));
        match result {
            Ok(false) => {
                log::info!(
                    "lanes: {} no longer holds run {}, not starting it",
                    pending.lane,
                    pending.job.run_id
                );
                Ok(false)
            }
            Ok(true) => {
                if staleness == Staleness::Stale {
                    pending.job.workflow = refresh.refresh.workflow;
                }
                Ok(true)
            }
            Err(error) => {
                log::error!(
                    "lanes: recording the staleness of {}: {error:#}",
                    refresh.task
                );
                Err(Cleared::Wait(WaitReason::Other(format!(
                    "recording the staleness answer: {error:#}"
                ))))
            }
        }
    }

    /// Clears a job whose start was refused, then evaluates.
    fn clear_start(&self, pending: &PendingJob, cleared: &Cleared, at: ClearedAt, cx: &mut App) {
        let now = self.parts().host.now();
        let extensions = PickExtensions::extensions(cx);
        let result = with_engine(&self.parts().engine, || {
            self.parts()
                .writer
                .transact(|tx| clear_job(tx, pending, cleared, at, &extensions, &now))
        });
        if let Err(error) = result {
            log::error!(
                "lanes: clearing the refused job of {}: {error:#}",
                pending.lane
            );
        }
        self.schedule(false, cx);
    }

    /// Awaits the job hooks' `before_job`, then starts the run unless a hook refused or the job
    /// was dropped meanwhile.
    fn begin_hooks(&self, pending: PendingJob, setup: JobSetup, cx: &mut App) {
        let hcx = JobHookCx::new(
            pending.lane.clone(),
            pending.job.run_id.clone(),
            self.parts().writer.clone(),
        );
        let decision = JobHooks::before_job(&pending.lane, &pending.job, hcx.clone(), cx);
        let weak = self.downgrade();
        let lane = pending.lane.clone();
        let job = pending.job.clone();
        let task = cx.spawn(async move |cx| {
            let decision = decision.await;
            if let Some(inner) = weak.upgrade() {
                cx.update(|cx| Dispatcher(inner).hooks_decided(pending, setup, decision, cx));
            }
        });
        self.0.hooked.borrow_mut().insert(
            lane,
            HookedJob {
                job,
                hcx,
                phase: HookPhase::Before(task),
            },
        );
    }

    /// Every hook's `before_job` resolved (or one refused).
    fn hooks_decided(
        &self,
        pending: PendingJob,
        setup: JobSetup,
        decision: BeforeJob,
        cx: &mut App,
    ) {
        let before = {
            let mut hooked = self.0.hooked.borrow_mut();
            let Some(entry) = hooked
                .get_mut(&pending.lane)
                .filter(|entry| entry.job.run_id == pending.job.run_id)
            else {
                return;
            };
            entry.hcx.finish();
            std::mem::replace(&mut entry.phase, HookPhase::Deciding)
        };
        // This runs inside that task, at its end; it is let finish rather than cancelled.
        if let HookPhase::Before(task) = before {
            task.detach();
        }

        let state = self.parts().writer.read(read_state);
        // A job dropped meanwhile is dropped whatever the hooks decided; a hook that refused is
        // not owed an `after_job` either way.
        if let Ok(state) = &state
            && self.drops(state, &pending.lane, &pending.job.run_id)
        {
            self.drop_hooked(&pending.lane, cx);
            return;
        }

        if let BeforeJob::Refuse { reason } = decision {
            let cleared = Cleared::Wait(reason);
            let at = ClearedAt::NotStarted;
            if self.owes_after_job(&pending.lane) {
                // An earlier hook said `Go`; it hears that the job did not start.
                self.settle_then_clear(pending, HookEnd::StoppedBeforeStart, cleared, at, cx);
            } else {
                self.0.hooked.borrow_mut().remove(&pending.lane);
                self.clear_start(&pending, &cleared, at, cx);
            }
            return;
        }

        if let Err(error) = state {
            log::error!("lanes: reading the state before a start: {error:#}");
            let cleared = Cleared::Wait(WaitReason::Other(format!(
                "reading the state before a start: {error:#}"
            )));
            let at = ClearedAt::NotStarted;
            self.settle_then_clear(pending, HookEnd::StoppedBeforeStart, cleared, at, cx);
            return;
        }
        if let Err(error) = self
            .parts()
            .writer
            .transact(|tx| resume_waiting(tx, &pending.lane, &pending.job.run_id))
        {
            log::error!("lanes: showing {} running: {error:#}", pending.lane);
        }
        match self.start_run(&pending, setup, cx) {
            None => {
                if let Some(entry) = self.0.hooked.borrow_mut().get_mut(&pending.lane) {
                    entry.phase = HookPhase::Started(Lifecycle::Created);
                }
            }
            Some(cleared) => {
                let reason = cleared.text();
                let end = HookEnd::Refused { reason };
                self.settle_then_clear(pending, end, cleared, ClearedAt::Refused, cx);
            }
        }
    }

    /// Tells the hooks owed an `after_job` that the job ended with `end`, then clears the job.
    fn settle_then_clear(
        &self,
        pending: PendingJob,
        end: HookEnd,
        cleared: Cleared,
        at: ClearedAt,
        cx: &mut App,
    ) {
        let lane = pending.lane.clone();
        self.settle(
            &lane,
            end,
            Some(Box::new(move |this, cx| {
                this.clear_start(&pending, &cleared, at, cx)
            })),
            cx,
        );
    }

    fn owes_after_job(&self, lane: &LaneId) -> bool {
        self.0
            .hooked
            .borrow()
            .get(lane)
            .is_some_and(|entry| entry.hcx.owes())
    }

    /// Whether the job `run_id` of `lane`, not started yet, is dropped: the lane no longer holds
    /// it, a stop was asked for, or the lane left the lane count.
    fn drops(&self, state: &LanesState, lane: &LaneId, run_id: &str) -> bool {
        let Some(record) = state.lanes.get(lane) else {
            return true;
        };
        !lane_holds(state, lane, run_id)
            || record.stop_requested.is_some()
            || !self.lane_exists(lane)
    }

    /// Drops a job whose run has not started: a pending `before_job` future is dropped, the hooks
    /// owed an `after_job` hear [`HookEnd::StoppedBeforeStart`], then the job is cleared.
    fn drop_hooked(&self, lane: &LaneId, cx: &mut App) {
        let (before, run_id) = {
            let mut hooked = self.0.hooked.borrow_mut();
            let Some(entry) = hooked.get_mut(lane) else {
                return;
            };
            entry.hcx.finish();
            let before = std::mem::replace(&mut entry.phase, HookPhase::Deciding);
            (before, entry.job.run_id.clone())
        };
        drop(before);
        log::info!("lanes: {lane} dropped run {run_id} before it started");
        let lane_id = lane.clone();
        self.settle(
            lane,
            HookEnd::StoppedBeforeStart,
            Some(Box::new(move |this, cx| {
                let now = this.parts().host.now();
                let extensions = PickExtensions::extensions(cx);
                let result = with_engine(&this.parts().engine, || {
                    this.parts()
                        .writer
                        .transact(|tx| drop_job(tx, &lane_id, &run_id, &extensions, &now))
                });
                if let Err(error) = result {
                    log::error!("lanes: clearing the dropped job of {lane_id}: {error:#}");
                }
                this.schedule(false, cx);
            })),
            cx,
        );
    }

    /// Calls `after_job` of every hook owed one for `lane`'s job, then `then`; the lane is
    /// evaluated again only after both.
    fn settle(&self, lane: &LaneId, end: HookEnd, then: Option<Then>, cx: &mut App) {
        let (owed, job) = {
            let mut hooked = self.0.hooked.borrow_mut();
            let Some(entry) = hooked.get_mut(lane) else {
                return;
            };
            entry.phase = HookPhase::After;
            (entry.hcx.take_owed(), entry.job.clone())
        };
        let done = after_job_of(owed, lane, &job, &end, cx);
        let weak = self.downgrade();
        let lane = lane.clone();
        cx.spawn(async move |cx| {
            done.await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            cx.update(|cx| {
                let this = Dispatcher(inner);
                if let Some(then) = then {
                    then(&this, cx);
                }
                let mut hooked = this.0.hooked.borrow_mut();
                if hooked
                    .get(&lane)
                    .is_some_and(|entry| entry.job.run_id == job.run_id)
                {
                    hooked.remove(&lane);
                }
                drop(hooked);
                this.schedule(false, cx);
            });
        })
        .detach();
    }

    /// At the start of a pass: drops the jobs still in `before_job` that `state` no longer lets
    /// start, and ends the hooked jobs whose lane no longer holds their run.
    pub(crate) fn sweep_hooked(&self, state: &LanesState, cx: &mut App) {
        let mut dropped = Vec::new();
        let mut ended = Vec::new();
        for (lane, entry) in self.0.hooked.borrow().iter() {
            match &entry.phase {
                HookPhase::Before(_) => {
                    if self.drops(state, lane, &entry.job.run_id) {
                        dropped.push(lane.clone());
                    }
                }
                HookPhase::Started(lifecycle) => {
                    if !lane_holds(state, lane, &entry.job.run_id) {
                        ended.push((lane.clone(), *lifecycle));
                    }
                }
                HookPhase::Deciding | HookPhase::After => {}
            }
        }
        for lane in dropped {
            self.drop_hooked(&lane, cx);
        }
        for (lane, lifecycle) in ended {
            self.settle(&lane, HookEnd::Lifecycle(lifecycle), None, cx);
        }
    }

    /// Settles the hooked job of `run` once its lane no longer holds it. It does not wait for a
    /// pass, since none runs once the dispatcher's run ended.
    fn settle_if_ended(&self, run: &str, cx: &mut App) {
        let started = self
            .0
            .hooked
            .borrow()
            .iter()
            .find_map(|(lane, entry)| match entry.phase {
                HookPhase::Started(lifecycle) if entry.job.run_id == run => {
                    Some((lane.clone(), lifecycle))
                }
                _ => None,
            });
        let Some((lane, lifecycle)) = started else {
            return;
        };
        match self.parts().writer.read(read_state) {
            Ok(state) if !lane_holds(&state, &lane, run) => {
                self.settle(&lane, HookEnd::Lifecycle(lifecycle), None, cx);
            }
            Ok(_) => {}
            // The next pass's sweep settles it.
            Err(error) => log::error!("lanes: reading the state after run {run}: {error:#}"),
        }
    }

    /// Records the state a hooked job's run reported.
    fn note_lifecycle(&self, run: &str, status: RunStatus) {
        for entry in self.0.hooked.borrow_mut().values_mut() {
            if entry.job.run_id == run
                && let HookPhase::Started(lifecycle) = &mut entry.phase
            {
                *lifecycle = lifecycle_of(status);
            }
        }
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
        self.note_lifecycle(run, *state);
        if *state == RunStatus::Succeeded && self.is_ahead_job(run) {
            self.await_ahead_landing(run, cx);
            return;
        }
        self.apply_job_change(run, *state, None, cx);
    }

    /// Whether `run` is the job of a running ahead lane that no pick extension assigned: its
    /// success waits for the landings it asked for.
    fn is_ahead_job(&self, run: &str) -> bool {
        // On an error the change is applied at once, which reads the state again and logs it.
        self.ahead_job_runs()
            .is_ok_and(|runs| runs.iter().any(|ahead| ahead == run))
    }

    /// The runs of the running ahead lanes' jobs that no pick extension assigned.
    fn ahead_job_runs(&self) -> Result<Vec<String>> {
        let table = self.table();
        let state = self.parts().writer.read(read_state)?;
        Ok(state
            .lanes
            .values()
            .filter(|lane| {
                lane.state == LaneState::Running && lane_kind_is_ahead(&table, &lane.id())
            })
            .filter_map(|lane| lane.job.as_ref())
            .filter(|job| job.extension.is_none())
            .map(|job| job.run_id.clone())
            .collect())
    }

    /// Waits again for the landings of every ahead job whose run already succeeded. A dispatcher
    /// that starts (the app restarted, or the lanes were started again) hears no event of a
    /// success that came before it, and the earlier dispatcher's wait went with it.
    pub(crate) fn resume_ahead_landings(&self, cx: &mut App) {
        let runs = match self.ahead_job_runs() {
            Ok(runs) => runs,
            Err(error) => {
                log::error!("lanes: reading the ahead jobs to wait on: {error:#}");
                return;
            }
        };
        for run in runs {
            if self.parts().engine.run_status(&run) == Some(RunStatus::Succeeded) {
                self.await_ahead_landing(&run, cx);
            }
        }
    }

    /// An ahead job's run succeeded: its lane keeps the job and its owner stays `Ahead` until
    /// every landing the run asked for resolved; then the end is applied with what they came to.
    /// One wait per run; the dispatcher holds it, so dropping the dispatcher ends it.
    fn await_ahead_landing(&self, run: &str, cx: &mut App) {
        if self.0.ahead_landings.borrow().contains_key(run) {
            return;
        }
        let landing = self.parts().host.ahead_landing(run, cx);
        let weak = self.downgrade();
        let key = run.to_string();
        let run = key.clone();
        let wait = cx.spawn(async move |cx| {
            let landing = landing.await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            // This task is running: detached, not dropped.
            if let Some(wait) = inner.ahead_landings.borrow_mut().remove(&run) {
                wait.detach();
            }
            cx.update(|cx| {
                Dispatcher(inner).apply_job_change(&run, RunStatus::Succeeded, Some(landing), cx)
            });
        });
        self.0.ahead_landings.borrow_mut().insert(key, wait);
    }

    /// Applies `run`'s status change to its lane and owner, then evaluates. `landing` is what an
    /// ahead job's landings came to; `None` for any other change.
    fn apply_job_change(
        &self,
        run: &str,
        state: RunStatus,
        landing: Option<AheadLanding>,
        cx: &mut App,
    ) {
        let graph = self.parts().host.graph(cx);
        let now = self.parts().host.now();
        let table = self.table();
        let master = self.verified();
        let extensions = PickExtensions::extensions(cx);
        let result = with_engine(&self.parts().engine, || {
            self.parts().writer.transact(|tx| {
                let end = JobChangeInput {
                    graph: graph.as_ref(),
                    table: &table,
                    master: &master,
                    landing: landing.as_ref(),
                    now: &now,
                };
                job_changed(tx, run, state, &end, &extensions)
            })
        });
        let ended = matches!(
            state,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Halted | RunStatus::Cancelled
        );
        match result {
            Ok(Some(lane)) => {
                if matches!(state, RunStatus::Succeeded | RunStatus::Cancelled) {
                    CurrentJobTrees::clear(&lane, cx);
                }
                if ended {
                    self.parts().host.snapshot_after_job(&lane, run, state, cx);
                }
            }
            Ok(None) => {}
            Err(error) => log::error!("lanes: the change of run {run}: {error:#}"),
        }
        self.settle_if_ended(run, cx);
        self.schedule(false, cx);
    }
}

/// Whether lanes of `kind` prepare ahead of their task's turn.
fn lane_kind_is_ahead(table: &LaneTable, lane: &LaneId) -> bool {
    table
        .kinds
        .iter()
        .any(|kind| kind.name == lane.kind && kind.ahead.is_some())
}

/// What lane leaves its job and the job's task with when the landing its ahead run asked for was
/// refused: both halt with the reason.
fn halted_by_landing(
    owner: &OwnerRecord,
    lane: &LaneRecord,
    run: &str,
    reason: &str,
    now: &str,
) -> Transition {
    let text = format!("landing refused: {reason}");
    let mut lane = lane.clone();
    lane.state = LaneState::Halted;
    lane.job = None;
    lane.reason = Some(WaitReason::Failed(text.clone()));
    lane.jobs_done += 1;
    let mut owner = owner.clone();
    owner.state = OwnerState::Halted;
    owner.reason = Some(text.clone());
    owner.by = Author::System;
    owner.since = now.to_string();
    Transition {
        owner,
        lane,
        history: Some(format!("run {} succeeded, but its {text}", short_run(run))),
    }
}

/// Clears a job whose start was refused, when the lane still holds it. An extension job's
/// extensions hear the change first and, when one answers, decide the lane instead.
fn clear_job(
    tx: &mut StateTx,
    pending: &PendingJob,
    cleared: &Cleared,
    at: ClearedAt,
    extensions: &[Arc<dyn PickExtension>],
    now: &str,
) -> Result<()> {
    let state = read_state_in(tx)?;
    let Some(lane) = state.lanes.get(&pending.lane) else {
        return Ok(());
    };
    let Some(job) = lane
        .job
        .as_ref()
        .filter(|job| job.run_id == pending.job.run_id)
    else {
        return Ok(());
    };
    let reason = cleared.text();
    let change = match at {
        ClearedAt::NotStarted => JobChange::NotStarted { reason },
        ClearedAt::Refused => JobChange::Refused { reason },
    };
    if let Some(after) = extension_job_changed(extensions, tx, &pending.lane, job, change) {
        return lane_after_unstarted(tx, &pending.lane, &job.run_id, after, now);
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

/// The staleness check of `pending`'s job in the trees and environment of `setup`.
fn staleness_check(
    pending: &PendingJob,
    refresh: &RefreshFirst,
    setup: &JobSetup,
) -> StalenessCheck {
    let tree = match (&setup.trees, &setup.tree) {
        (Some(trees), _) => Some(trees.primary().to_path_buf()),
        (None, bongos_workflow::RunTree::Lane { path, .. }) => Some(path.clone()),
        (None, _) => None,
    };
    StalenessCheck {
        lane: pending.lane.clone(),
        task: refresh.task.clone(),
        prepared_commit: refresh.prepared_commit.clone(),
        script: refresh.refresh.staleness.clone(),
        definition_commit: pending.job.definition_commit.clone(),
        tree,
        env: setup.env.clone(),
    }
}

/// Records a staleness answer for a job `pending.lane` still holds: the owner is refreshed, and
/// on `stale` the lane's job runs the refresh workflow, noted in the task's history. Returns
/// whether the lane still holds the job.
fn record_staleness(
    tx: &mut StateTx,
    pending: &PendingJob,
    refresh: &RefreshFirst,
    staleness: Staleness,
    now: &str,
) -> Result<bool> {
    let state = read_state_in(tx)?;
    if !lane_holds(&state, &pending.lane, &pending.job.run_id) {
        return Ok(false);
    }
    if let Some(owner) = state.owners.get(&refresh.task) {
        let mut owner = owner.clone();
        owner.refreshed = true;
        put_owner(tx, &refresh.task, &owner)?;
    }
    let event = match staleness {
        Staleness::Stale => {
            let mut lane = state.lanes[&pending.lane].clone();
            if let Some(job) = &mut lane.job {
                job.workflow = refresh.refresh.workflow.clone();
            }
            put_lane(tx, &lane)?;
            format!(
                "staleness script says stale; {} runs {} first",
                pending.lane, refresh.refresh.workflow
            )
        }
        Staleness::Fresh => "staleness script says fresh".to_string(),
    };
    append_history(tx, &refresh.task, &event, now)?;
    Ok(true)
}

/// Shows a lane that a hook's `set_wait` showed waiting as running again, when it still holds
/// the job about to start. A requested pause keeps its reason.
fn resume_waiting(tx: &mut StateTx, lane: &LaneId, run_id: &str) -> Result<()> {
    let state = read_state_in(tx)?;
    if !lane_holds(&state, lane, run_id) {
        return Ok(());
    }
    let record = &state.lanes[lane];
    if record.state != LaneState::Waiting {
        return Ok(());
    }
    let mut updated = record.clone();
    updated.state = LaneState::Running;
    if !updated.pause_requested {
        updated.reason = None;
    }
    put_lane(tx, &updated)
}

/// Clears a job dropped before its run started, when the lane still holds it. The lane is idle
/// again, or stopped with its owner when a stop was asked for; the owner record stays. An
/// extension job's extensions hear [`JobChange::NotStarted`] first and, when one answers, decide
/// the lane instead.
fn drop_job(
    tx: &mut StateTx,
    lane: &LaneId,
    run_id: &str,
    extensions: &[Arc<dyn PickExtension>],
    now: &str,
) -> Result<()> {
    let state = read_state_in(tx)?;
    if !lane_holds(&state, lane, run_id) {
        return Ok(());
    }
    if let Some(job) = &state.lanes[lane].job {
        let change = JobChange::NotStarted {
            reason: DROPPED_BEFORE_START.to_string(),
        };
        if let Some(after) = extension_job_changed(extensions, tx, lane, job, change) {
            return lane_after_unstarted(tx, lane, run_id, after, now);
        }
    }
    let mut updated = state.lanes[lane].clone();
    updated.job = None;
    updated.state = LaneState::Idle;
    updated.reason = None;
    let held = held_task(&state, lane);
    let (updated, owner) = stop_at_job_boundary(&updated, held.map(|(_, owner)| owner), now);
    put_lane(tx, &updated)?;
    if let (Some((task, before)), Some(owner)) = (held, owner)
        && *before != owner
    {
        put_owner(tx, task, &owner)?;
    }
    Ok(())
}

/// The lifecycle a run status reports.
fn lifecycle_of(status: RunStatus) -> Lifecycle {
    match status {
        RunStatus::Queued => Lifecycle::Queued,
        RunStatus::Running | RunStatus::WaitingOnYou => Lifecycle::Running,
        RunStatus::Pausing | RunStatus::Paused => Lifecycle::Paused,
        RunStatus::Halted => Lifecycle::Halted,
        RunStatus::Succeeded => Lifecycle::Ended(EndState::Succeeded),
        RunStatus::Failed => Lifecycle::Ended(EndState::Failed),
        RunStatus::Cancelled => Lifecycle::Cancelled,
    }
}

/// What a run's status change is applied with.
struct JobChangeInput<'a> {
    graph: Option<&'a PickGraph>,
    table: &'a LaneTable,
    /// The verified commit, which an ahead job's success leaves its task prepared at when its
    /// run landed nothing it waited for.
    master: &'a str,
    /// What an ahead job's landings came to, once resolved.
    landing: Option<&'a AheadLanding>,
    now: &'a str,
}

/// Applies a run's status change to the lane whose job it is. Returns that lane.
fn job_changed(
    tx: &mut StateTx,
    run: &str,
    status: RunStatus,
    input: &JobChangeInput<'_>,
    extensions: &[Arc<dyn PickExtension>],
) -> Result<Option<LaneId>> {
    let JobChangeInput {
        graph,
        table,
        master,
        landing,
        now,
    } = *input;
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
    if let Some(job) = &lane.job {
        let change = JobChange::Lifecycle(lifecycle_of(status));
        if let Some(after) = extension_job_changed(extensions, tx, &id, job, change) {
            lane_after_run(tx, &id, run, after, status, now)?;
            return Ok(Some(id));
        }
    }
    let lane_kind_is_ahead = lane_kind_is_ahead(table, &id);
    let all_leaves_finished = match (&held, graph) {
        (Some((task, _)), Some(graph)) => all_finished(graph, task),
        _ => false,
    };
    let owner = held
        .as_ref()
        .map(|(_, owner)| owner.clone())
        .unwrap_or_else(|| placeholder_owner(now));
    let succeeded = outcome == JobEnd::Succeeded;
    let transition = match landing {
        Some(AheadLanding::Refused(reason)) if succeeded && lane_kind_is_ahead => {
            halted_by_landing(&owner, &lane, run, reason, now)
        }
        _ => {
            // An ahead job's success leaves its task prepared at the commit its landings
            // reached, else at the verified commit.
            let prepared_commit = match landing {
                Some(AheadLanding::Landed { commit }) => commit.clone(),
                _ => master.to_string(),
            };
            let input = JobEndInput {
                outcome,
                lane_kind_is_ahead,
                all_leaves_finished,
                checks_owed: false,
                prepared_commit: lane_kind_is_ahead.then_some(prepared_commit),
                now,
            };
            owner_after_job_end(&owner, &lane, &input)
        }
    };
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

/// The task of a lane's job and its owner record; none for a job with no task. Owner records
/// are kept per top-level task, so a job on a leaf below one has the owner record that names its
/// lane.
pub(crate) fn job_owner(state: &LanesState, lane: &LaneRecord) -> Option<(TaskId, OwnerRecord)> {
    let RunSubject::Task(task) = &lane.job.as_ref()?.subject else {
        return None;
    };
    if let Some(owner) = state.owners.get(task) {
        return Some((task.clone(), owner.clone()));
    }
    let (task, owner) = held_task(state, &lane.id())?;
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
