//! Pick extensions: other features (pools, queues, triggers) taking part in a lane's pick and
//! settling the jobs they assign. With none registered every call is a no-op and the picker
//! alone decides.
//!
//! In the transaction that evaluates a free lane, every registered [`PickExtension::before_pick`]
//! runs before the picker, and every [`PickExtension::after_pick`] after a pick that found
//! nothing; the first answer other than [`ExtensionPick::Pass`] decides. An assignment an
//! extension returns commits in that same transaction as the lane's job, so the extension's own
//! claim and the job commit together. Every change of an extension job (its work is
//! `AssignedWork::Extension`) goes to [`PickExtension::job_changed`] in the transaction that
//! records it, instead of the default end and refusal handling, whenever an extension answers.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use bongos_lanes_model::{
    Assignment, Author, InstanceUse, JobRecord, LaneId, LaneState, LanesState, OwnerState,
    WaitReason, stop_at_job_boundary,
};
use bongos_project_repo::StateTx;
use bongos_workflow::RunStatus;
use bongos_workflow_model::record::Lifecycle;
use gpui::{App, Global};

use crate::dispatch_job::job_owner;
use crate::dispatcher::{JobEngine, held_task};
use crate::store::{put_lane, put_owner, read_state, read_state_in};

/// What a pick extension answers for a free lane.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionPick {
    /// This becomes the lane's job; its `work` is `AssignedWork::Extension`. With `run_id` set the
    /// job's run gets exactly that id.
    Assign(Assignment),
    /// The lane stops with [`bongos_lanes_model::StopCause::Extension`], showing the text.
    StopLane(String),
    /// The next extension, or the picker, decides.
    Pass,
}

/// A change of an extension job, as [`PickExtension::job_changed`] hears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobChange {
    /// The job's run ended or halted in this state.
    Lifecycle(Lifecycle),
    /// The run engine refused to start the job's run.
    Refused { reason: String },
    /// The job was cleared before it reached the run engine: a job hook refused it, its trees
    /// could not be prepared, it was dropped (a stop, a count or lane table change, a handover),
    /// or a crash came between the pick and the start.
    NotStarted { reason: String },
}

/// What becomes of the lane after a change of its extension job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneAfterJob {
    /// The job is cleared and the lane is idle (or stopped, when a stop was asked for).
    Idle,
    /// The lane keeps the job and stays running while its run can still be retried (a failed or
    /// halted run): the extension expects another change of the same run. When the run cannot
    /// come back (it succeeded or was cancelled, was refused or never started) this is
    /// [`LaneAfterJob::Idle`].
    KeepRunning,
    /// The lane halts with this text, and so does the job's own task (none for a job with no
    /// task; another task the lane holds is left alone). A run that can be retried stays the
    /// lane's job, so a retry resumes the lane.
    Halt(String),
}

/// One feature's part in the picks. Each default does nothing.
///
/// Every call runs inside the dispatcher's transaction: what an extension writes with `tx`
/// commits with the lane's records, or not at all.
pub trait PickExtension {
    /// Runs before the picker for a free lane. The first extension to answer other than
    /// [`ExtensionPick::Pass`] decides, and neither the later ones nor the picker are asked.
    fn before_pick(&self, _tx: &mut StateTx, _lane: &LaneId) -> ExtensionPick {
        ExtensionPick::Pass
    }

    /// Runs only after the picker found nothing for the lane (not after an assignment or a
    /// hold); the first answer other than [`ExtensionPick::Pass`] decides.
    fn after_pick(&self, _tx: &mut StateTx, _lane: &LaneId) -> ExtensionPick {
        ExtensionPick::Pass
    }

    /// Runs for every change of an extension job, in the transaction that records it. The first
    /// extension to return `Some` decides; when all return `None` the default handling applies.
    fn job_changed(
        &self,
        _tx: &mut StateTx,
        _lane: &LaneId,
        _job: &JobRecord,
        _change: JobChange,
    ) -> Option<LaneAfterJob> {
        None
    }

    /// Lines the dry run adds to its buffer for this extension (for example the pool or queue
    /// that would hold a lane). Reads `state` only and writes nothing.
    fn explain(&self, _state: &LanesState) -> Vec<String> {
        Vec::new()
    }
}

/// The registered pick extensions, asked in the order they were registered.
#[derive(Default)]
pub struct PickExtensions {
    extensions: Vec<Arc<dyn PickExtension>>,
}

impl Global for PickExtensions {}

impl PickExtensions {
    pub fn register(extension: Arc<dyn PickExtension>, cx: &mut App) {
        cx.default_global::<Self>().extensions.push(extension);
    }

    /// Every registered extension.
    pub fn extensions(cx: &App) -> Vec<Arc<dyn PickExtension>> {
        cx.try_global::<Self>()
            .map(|extensions| extensions.extensions.clone())
            .unwrap_or_default()
    }
}

/// Where in a pick the extensions are asked.
#[derive(Clone, Copy)]
pub(crate) enum PickPoint {
    Before,
    After,
}

/// The first answer other than `Pass` of `extensions` at `point`.
pub(crate) fn extension_pick(
    extensions: &[Arc<dyn PickExtension>],
    point: PickPoint,
    tx: &mut StateTx,
    lane: &LaneId,
) -> ExtensionPick {
    for extension in extensions {
        let pick = match point {
            PickPoint::Before => extension.before_pick(tx, lane),
            PickPoint::After => extension.after_pick(tx, lane),
        };
        if pick != ExtensionPick::Pass {
            return pick;
        }
    }
    ExtensionPick::Pass
}

/// The first `Some` of `extensions` for a change of `job`; `None` for a job no extension
/// assigned.
pub(crate) fn extension_job_changed(
    extensions: &[Arc<dyn PickExtension>],
    tx: &mut StateTx,
    lane: &LaneId,
    job: &JobRecord,
    change: JobChange,
) -> Option<LaneAfterJob> {
    job.extension.as_ref()?;
    extensions
        .iter()
        .find_map(|extension| extension.job_changed(tx, lane, job, change.clone()))
}

thread_local! {
    /// The engines of the dispatcher transactions running now, innermost last.
    static ENGINES: RefCell<Vec<Rc<dyn JobEngine>>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with `engine` as the one [`instance_room`] asks about its runs.
pub(crate) fn with_engine<R>(engine: &Rc<dyn JobEngine>, f: impl FnOnce() -> R) -> R {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            ENGINES.with(|engines| engines.borrow_mut().pop());
        }
    }
    ENGINES.with(|engines| engines.borrow_mut().push(engine.clone()));
    let _pop = Pop;
    f()
}

/// The use of `workflow`'s instances: its limit, its active runs, and the lane jobs of it whose
/// reserved run has not started yet (a job still being prepared or in `before_job`).
pub(crate) fn instance_use(
    engine: &dyn JobEngine,
    state: &LanesState,
    workflow: &str,
) -> InstanceUse {
    let reserved = state
        .lanes
        .values()
        .filter_map(|lane| lane.job.as_ref())
        .filter(|job| job.workflow == workflow && !engine.knows(&job.run_id))
        .count() as u32;
    InstanceUse {
        limit: engine.instance_limit(workflow),
        active_runs: engine.active_runs_of(workflow),
        reserved,
    }
}

/// Whether one more run of the workflow definition `workflow` fits its `instances`: the active
/// runs of it, the reserved lane jobs of it not started yet, and the lane jobs assigned earlier in
/// the same transaction (they are lane jobs `tx` sees) are counted against the limit. The
/// dispatcher's own picks use the same count, so an extension applies the same limit.
///
/// Call it only inside a dispatcher's transaction (the calls to a [`PickExtension`]), where the
/// run engine is known. Called anywhere else it knows no limit: that is a caller's mistake, so it
/// logs an error naming the workflow and answers `Ok`, as with no limit.
pub fn instance_room(tx: &StateTx, workflow: &str) -> Result<(), WaitReason> {
    let Some(engine) = ENGINES.with(|engines| engines.borrow().last().cloned()) else {
        log::error!(
            "lanes: instance_room for {workflow} was called outside a dispatcher transaction, \
             so no instance limit was checked"
        );
        return Ok(());
    };
    let state = read_state(&tx.as_read())
        .map_err(|error| WaitReason::Other(format!("reading the lanes state: {error:#}")))?;
    instance_use(&*engine, &state, workflow).room(workflow, 0)
}

/// Applies an extension's answer for a job cleared before its run started (refused or not
/// started): the job is cleared, and the lane is idle, or halted with its task on
/// [`LaneAfterJob::Halt`]. A stop asked for applies at once. Does nothing when the lane no longer
/// holds the job.
pub(crate) fn lane_after_unstarted(
    tx: &mut StateTx,
    lane: &LaneId,
    run_id: &str,
    after: LaneAfterJob,
    now: &str,
) -> Result<()> {
    lane_after(tx, lane, run_id, after, false, false, now)
}

/// Applies an extension's answer for a run of its job that left the lane: `status` is the run's.
pub(crate) fn lane_after_run(
    tx: &mut StateTx,
    lane: &LaneId,
    run_id: &str,
    after: LaneAfterJob,
    status: RunStatus,
    now: &str,
) -> Result<()> {
    let retryable = matches!(status, RunStatus::Failed | RunStatus::Halted);
    let succeeded = status == RunStatus::Succeeded;
    lane_after(tx, lane, run_id, after, retryable, succeeded, now)
}

fn lane_after(
    tx: &mut StateTx,
    lane: &LaneId,
    run_id: &str,
    after: LaneAfterJob,
    retryable: bool,
    succeeded: bool,
    now: &str,
) -> Result<()> {
    // The extension may have written in this transaction; the records are read after it.
    let state = read_state_in(tx)?;
    let Some(record) = state.lanes.get(lane) else {
        return Ok(());
    };
    if record.job.as_ref().is_none_or(|job| job.run_id != run_id) {
        return Ok(());
    }
    let held = held_task(&state, lane);
    let mut updated = record.clone();
    let mut halted = None;
    match after {
        LaneAfterJob::KeepRunning if retryable => {
            updated.state = LaneState::Running;
            if !updated.pause_requested {
                updated.reason = None;
            }
        }
        LaneAfterJob::Idle | LaneAfterJob::KeepRunning => {
            updated.job = None;
            updated.state = LaneState::Idle;
            if !updated.pause_requested {
                updated.reason = None;
            }
            if succeeded {
                updated.jobs_done += 1;
            }
        }
        LaneAfterJob::Halt(text) => {
            if !retryable {
                updated.job = None;
            }
            updated.state = LaneState::Halted;
            updated.reason = Some(WaitReason::Failed(text.clone()));
            // Only the job's own task halts with the lane, as with the default handling: a lane
            // that still holds another task's owner (a free lane given a job with no task, or one
            // for another task) leaves that owner alone, so a retry of the job resumes the lane
            // and no owner is left halted with nothing to resume it.
            if let Some((task, mut owner)) = job_owner(&state, record) {
                owner.state = OwnerState::Halted;
                owner.reason = Some(text);
                owner.by = Author::System;
                owner.since = now.to_string();
                halted = Some((task, owner));
            }
        }
    }
    // A stop asked for applies to the task the lane holds, at the boundary (an idle lane only).
    let (mut updated, stopped) = stop_at_job_boundary(&updated, held.map(|(_, owner)| owner), now);
    if updated.state == LaneState::Stopped {
        updated.pause_requested = false;
    }
    if updated != *record {
        put_lane(tx, &updated)?;
    }
    if let (Some((task, before)), Some(owner)) = (held, stopped)
        && *before != owner
    {
        put_owner(tx, task, &owner)?;
    }
    if let Some((task, owner)) = halted
        && state.owners.get(&task) != Some(&owner)
    {
        put_owner(tx, &task, &owner)?;
    }
    Ok(())
}
