//! `lanes: adopt`: a person gives a stopped or halted task to a free lane.
//!
//! The checks run first, against the lanes state: a task whose owner is active on a running lane,
//! a pin that sends the task to another lane or kind, a target lane that owns a task or is not
//! idle, and an empty reason are refused. A halted task's leaf still has its failed run, which
//! the engine keeps active until it is cancelled. Before that run is cancelled, the work it left
//! uncommitted in its lane's tree is saved by the rule of the save before a reset (one commit on
//! the run's branch, or at `refs/bongos/saved/<lane>/<run id>`), and the task's history names
//! where. A failed save cancels and moves nothing. A tree that cannot be read at all refuses the
//! adopt unless the person confirms dropping its work, which the history records. Then the run is
//! cancelled, and one transaction moves the owner to the target lane, makes the source lane idle
//! and appends the history entry with the reason and the author. The target lane's next job
//! starts from the task branch head in its own tree, like any job start. A cancelled run cannot
//! be retried, so the old lane never takes the task back.

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_lanes_model::{
    AdoptRefusal, Author, JobRecord, LaneId, LaneTable, PickGraph, PinTarget, TaskId, short_run,
};
use bongos_workflow::worktrees::Git;
use gpui::{App, Task};

use crate::dispatch_job::{append_history, append_history_by};
use crate::dispatcher::{Dispatcher, Lanes, LanesParts};
use crate::lane_tree::{ResetRequest, SaveError, save_left_work};
use crate::store::{put_lane, put_owner, read_state, read_state_in};

/// One persistent tree a halted job ran in, with what saving its work needs: the request's
/// `last` names the job's run (ended failed or halted), and its `run_branch` names no new run.
#[derive(Clone, Debug)]
pub struct LeftWork {
    pub git: Git,
    pub request: ResetRequest,
}

/// What `lanes: adopt` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptRequest {
    pub task: TaskId,
    /// The free lane that takes the task.
    pub lane: LaneId,
    /// Why; required.
    pub reason: String,
    pub by: Author,
    /// The person agreed to drop the work of a halted job whose tree cannot be read.
    pub confirm_drop: bool,
}

/// What an adopt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub task: TaskId,
    pub lane: LaneId,
    /// The lane the task came from.
    pub from: Option<LaneId>,
    /// The halted job's run, cancelled.
    pub cancelled: Option<String>,
    /// The history entries of the saves made before the cancel.
    pub saved: Vec<String>,
    /// The halted job's tree could not be read and the person dropped its work.
    pub dropped_work: bool,
}

impl Lanes {
    /// Gives `request.task` to `request.lane`. See the module documentation.
    pub fn adopt(&self, request: AdoptRequest, cx: &mut App) -> Task<Result<Adopted>> {
        adopt(self.parts.clone(), request, cx)
    }
}

impl Dispatcher {
    /// [`Lanes::adopt`], then a pass so the target lane starts at once.
    pub fn adopt(&self, request: AdoptRequest, cx: &mut App) -> Task<Result<Adopted>> {
        let adopting = adopt(self.parts().clone(), request, cx);
        let this = self.clone();
        cx.spawn(async move |cx| {
            let adopted = adopting.await?;
            cx.update(|cx| this.pick_now(cx));
            Ok(adopted)
        })
    }
}

/// What the checks found before anything is written.
struct Checked {
    pin: Option<PinTarget>,
    /// The halted job whose run the adopt cancels, on its lane.
    halted_job: Option<(LaneId, JobRecord)>,
    now: String,
}

fn adopt(parts: LanesParts, request: AdoptRequest, cx: &mut App) -> Task<Result<Adopted>> {
    let checked = match check(&parts, &request, cx) {
        Ok(checked) => checked,
        Err(error) => return Task::ready(Err(error)),
    };
    let left = match &checked.halted_job {
        Some((lane, job)) => parts.host.left_work(lane, job, cx),
        None => Vec::new(),
    };
    cx.spawn(async move |cx| {
        let mut saved = Vec::new();
        let mut unreadable = None;
        for work in &left {
            match save_left_work(&work.git, &work.request).await {
                Ok(Some(done)) => {
                    let line = done.left_on();
                    // Recorded now, before anything is cancelled: the history names where the
                    // work went even if what follows fails.
                    parts
                        .writer
                        .transact(|tx| append_history(tx, &request.task, &line, &checked.now))
                        .with_context(|| format!("recording the save: {line}"))?;
                    saved.push(line);
                }
                Ok(None) => {}
                Err(error @ SaveError::Unreadable { .. }) => unreadable = Some(error.to_string()),
                Err(SaveError::Failed(why)) => bail!(
                    "the work the failed run of {} left was not saved, so nothing was cancelled \
                     or moved: {why}",
                    request.task
                ),
            }
        }
        cx.update(|cx| finish(&parts, &request, &checked, saved, unreadable, cx))
    })
}

fn model_request(
    request: &AdoptRequest,
    now: &str,
    old_tree_readable: bool,
) -> bongos_lanes_model::AdoptRequest {
    bongos_lanes_model::AdoptRequest {
        task: request.task.clone(),
        lane: request.lane.clone(),
        reason: request.reason.clone(),
        by: request.by.clone(),
        since: now.to_string(),
        old_tree_readable,
        confirm_drop: request.confirm_drop,
    }
}

fn check(parts: &LanesParts, request: &AdoptRequest, cx: &App) -> Result<Checked> {
    let verified = parts
        .writer
        .read(|read| read.verified_commit())?
        .context("the project has no verified commit yet")?;
    let table = parts
        .host
        .lane_table(&verified)
        .map_err(|error| anyhow!("the lane table cannot be read: {error}"))?;
    let graph = parts.host.graph(cx);
    let pin = blocking_pin(&table, graph.as_ref(), &request.task, &request.lane);
    let now = parts.host.now();
    let state = parts.writer.read(read_state)?;
    let outcome =
        bongos_lanes_model::adopt(&state, &model_request(request, &now, true), pin.as_ref())?;
    let halted_job = outcome.cancel_run.and_then(|run| {
        let lane = state.owners.get(&request.task)?.lane.clone()?;
        let job = state.lanes.get(&lane)?.job.clone()?;
        (job.run_id == run).then_some((lane, job))
    });
    Ok(Checked {
        pin,
        halted_job,
        now,
    })
}

/// The first pin on the task or one of its ancestors that does not allow `lane`.
fn blocking_pin(
    table: &LaneTable,
    graph: Option<&PickGraph>,
    task: &TaskId,
    lane: &LaneId,
) -> Option<PinTarget> {
    let mut path = graph.map(|graph| graph.path(task)).unwrap_or_default();
    if !path.contains(task) {
        path.push(task.clone());
    }
    table
        .pins
        .iter()
        .filter(|pin| {
            pin.task
                .as_ref()
                .is_some_and(|pinned| path.iter().any(|step| step.0 == pinned.value))
        })
        .map(PinTarget::from)
        .find(|target| !target.allows(lane))
}

/// After the saves: the checks again, the cancel, then the ownership change in one transaction.
fn finish(
    parts: &LanesParts,
    request: &AdoptRequest,
    checked: &Checked,
    saved: Vec<String>,
    unreadable: Option<String>,
    cx: &mut App,
) -> Result<Adopted> {
    let model = model_request(request, &checked.now, unreadable.is_none());
    let state = parts.writer.read(read_state)?;
    let outcome = match bongos_lanes_model::adopt(&state, &model, checked.pin.as_ref()) {
        Ok(outcome) => outcome,
        Err(refusal @ AdoptRefusal::UnreadableTree(_)) => {
            bail!("{refusal} ({})", unreadable.unwrap_or_default())
        }
        Err(refusal) => return Err(refusal.into()),
    };
    let cancelled = outcome.cancel_run;
    if let Some(run) = &cancelled {
        parts
            .engine
            .cancel_run(run, cx)
            .map_err(|error| anyhow!("cancelling run {} failed: {error}", short_run(run)))?;
    }
    let now = &checked.now;
    let task = &request.task;
    let outcome = parts.writer.transact(|tx| {
        let state = read_state_in(tx)?;
        let outcome = bongos_lanes_model::adopt(&state, &model, checked.pin.as_ref())?;
        put_owner(tx, task, &outcome.owner)?;
        if let Some((_, lane)) = &outcome.source_lane {
            put_lane(tx, lane)?;
        }
        // Named from the run cancelled above: the lane may have heard of the cancel already.
        if let Some(run) = &cancelled {
            let line = format!(
                "run {} cancelled by adopt to {}",
                short_run(run),
                request.lane
            );
            append_history(tx, task, &line, now)?;
        }
        append_history_by(tx, task, &outcome.history, &request.by.to_string(), now)?;
        Ok(outcome)
    })?;
    Ok(Adopted {
        task: task.clone(),
        lane: request.lane.clone(),
        from: state.owners.get(task).and_then(|owner| owner.lane.clone()),
        cancelled,
        saved,
        dropped_work: outcome.dropped_work,
    })
}
