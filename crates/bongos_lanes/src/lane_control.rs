//! Lane control: pause, resume and stop, the events the dispatcher emits, and the status of the
//! lanes. Every command is one state transaction; the dispatcher's passes act on the records.
//!
//! A pause or a stop of a running lane is a request the lane applies at its job boundary. A lane
//! with no job boundary (halted, paused) keeps its state, task and reason when a stop is asked
//! for. The dispatcher's run ends when every lane is at rest (stopped, halted or paused) and every
//! job that went through the job hooks got its `after_job`: cancelled when `stop` with no lane
//! asked for it, succeeded when the lanes stopped for a bound.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use anyhow::{Result, bail};
use bongos_lanes_model::{
    Author, JobRecord, LaneId, LaneRecord, LaneState, LanesState, StopCause, TaskId, WaitReason,
    stop_at_job_boundary,
};
use bongos_project_repo::{StateRead, StateTx};
use futures::channel::mpsc;
use gpui::App;
use serde::{Deserialize, Serialize};

use crate::dispatcher::{
    Dispatcher, DispatcherRecord, Lanes, LanesParts, dispatcher_record, held_task, put_dispatcher,
    read_dispatcher_record,
};
use crate::lane_waits::{
    LaneAsk, ask_in, asked_text, asks_in, delete_ask, publish_asks, read_asks,
};
use crate::store::{put_lane, put_owner, read_state, read_state_in};

/// The reason of a lane that stopped for the bound of the start.
pub const BOUND_REASON: &str = "bound";
const PAUSE_REQUESTED: &str = "pause requested by ";
const PAUSED_BY: &str = "paused by ";

/// Which lanes a stop is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneTarget {
    All,
    Lane(LaneId),
}

impl LaneTarget {
    fn includes(&self, lane: &LaneId) -> bool {
        match self {
            LaneTarget::All => true,
            LaneTarget::Lane(target) => target == lane,
        }
    }
}

/// How the dispatcher's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEnd {
    Succeeded,
    Cancelled,
}

/// What the lanes tell the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneEvent {
    /// Every lane is at rest and the dispatcher's run ended.
    DispatcherEnded {
        run_id: String,
        end: RunEnd,
        note: String,
    },
}

/// The listeners of [`LaneEvent`]s. Cheap to clone; clones share their listeners.
#[derive(Clone, Default)]
pub struct LaneEvents(Rc<RefCell<Vec<mpsc::UnboundedSender<LaneEvent>>>>);

impl LaneEvents {
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<LaneEvent> {
        let (sender, receiver) = mpsc::unbounded();
        self.0.borrow_mut().push(sender);
        receiver
    }

    fn emit(&self, event: LaneEvent) {
        self.0
            .borrow_mut()
            .retain(|sender| sender.unbounded_send(event.clone()).is_ok());
    }
}

/// What the dispatcher run is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatcherPhase {
    Running,
    /// A stop was asked for; running lanes finish their job first.
    Pausing,
    Ended(RunEnd),
}

/// One lane as `status` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneStatus {
    pub id: LaneId,
    pub state: LaneState,
    pub reason: Option<WaitReason>,
    /// The task the lane holds.
    pub task: Option<TaskId>,
    pub pause_requested: bool,
    pub stop_requested: bool,
    /// What the lane asks of a person while it waits on one (a lane wait handler's ask), until
    /// `lanes: resume`.
    pub asked: Option<String>,
    /// The job the lane runs now.
    pub job: Option<JobRecord>,
    /// The jobs the lane finished.
    pub jobs_done: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanesStatus {
    /// `None` when no dispatcher was started.
    pub phase: Option<DispatcherPhase>,
    /// The commit the lane table is read at.
    pub table_commit: Option<String>,
    /// Why a newer lane table is not adopted, while the definition policy says to wait.
    pub table_wait: Option<String>,
    pub lanes: Vec<LaneStatus>,
}

impl Lanes {
    /// Events of the dispatchers this handle starts and of its commands.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<LaneEvent> {
        self.events.subscribe()
    }

    /// Asks `target` to stop. See [`LaneTarget`] and the module documentation.
    pub fn stop(&self, target: LaneTarget, by: Author, cx: &mut App) -> Result<()> {
        stop(&self.parts, target, by)?;
        // The dispatcher this handle runs decides its own end, since it knows its hooked jobs.
        match self.dispatcher() {
            Some(dispatcher) => {
                dispatcher.end_if_at_rest(cx);
            }
            None => {
                end_if_at_rest(&self.parts, &self.events, cx)?;
            }
        }
        Ok(())
    }

    /// Pauses `lane`: at once when idle, at its job boundary when running.
    pub fn pause(&self, lane: &LaneId, by: &Author) -> Result<String> {
        pause(&self.parts, lane, by)
    }

    /// Resumes a paused or stopped lane, withdraws a pause not yet applied, or clears the ask of a
    /// lane waiting on a person so it is evaluated again.
    pub fn resume(&self, lane: &LaneId) -> Result<String> {
        resume(&self.parts, lane)
    }

    pub fn status(&self) -> Result<LanesStatus> {
        status(&self.parts)
    }
}

impl Dispatcher {
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<LaneEvent> {
        self.0.config.events.subscribe()
    }

    pub fn stop(&self, target: LaneTarget, by: Author, cx: &mut App) -> Result<()> {
        stop(self.parts(), target, by)?;
        self.end_if_at_rest(cx);
        Ok(())
    }

    pub fn pause(&self, lane: &LaneId, by: &Author) -> Result<String> {
        pause(self.parts(), lane, by)
    }

    pub fn resume(&self, lane: &LaneId) -> Result<String> {
        resume(self.parts(), lane)
    }

    pub fn status(&self) -> Result<LanesStatus> {
        status(self.parts())
    }

    /// Ends the dispatcher's run when every lane is at rest and no job is in the job hooks (one
    /// still owed its `after_job`, or still in `before_job`). Returns whether it ended.
    pub(crate) fn end_if_at_rest(&self, cx: &mut App) -> bool {
        if !self.0.hooked.borrow().is_empty() {
            return false;
        }
        match end_if_at_rest(self.parts(), &self.0.config.events, cx) {
            Ok(ended) => {
                if ended {
                    // No pass runs any more, so nothing would withdraw the published asks.
                    publish_asks(&self.0.config.project, Vec::new(), cx);
                }
                ended
            }
            Err(error) => {
                log::error!("lanes: ending the dispatcher: {error:#}");
                false
            }
        }
    }
}

/// Records the stop of `target`; the caller then ends the dispatcher's run when it is at rest.
fn stop(parts: &LanesParts, target: LaneTarget, by: Author) -> Result<()> {
    let now = parts.host.now();
    parts.writer.transact(|tx| {
        let state = read_state_in(tx)?;
        for (id, lane) in &state.lanes {
            if target.includes(id) {
                stop_one(tx, &state, lane, &now)?;
            }
        }
        if target == LaneTarget::All
            && let Some(mut record) = dispatcher_record(tx)?
        {
            record.stopped_by = Some(by);
            put_dispatcher(tx, &record)?;
        }
        Ok(())
    })
}

/// A stop of one lane. A running lane, and a waiting lane that holds a reserved job, finish the
/// job first (a job still in its hooks' `before_job` is dropped by the next pass instead); an
/// idle lane or a waiting lane with no job stops at once; a halted or paused lane records the
/// request and stays as it is.
fn stop_one(tx: &mut StateTx, state: &LanesState, lane: &LaneRecord, now: &str) -> Result<()> {
    let mut updated = lane.clone();
    match lane.state {
        LaneState::Stopped => return Ok(()),
        LaneState::Running | LaneState::Halted | LaneState::Paused => {
            updated.stop_requested = Some(StopCause::Stop);
        }
        LaneState::Waiting if lane.job.is_some() => {
            updated.stop_requested = Some(StopCause::Stop);
        }
        LaneState::Idle | LaneState::Waiting => {
            delete_ask(tx, &lane.id())?;
            updated.state = LaneState::Idle;
            updated.reason = None;
            updated.stop_requested = Some(StopCause::Stop);
            let held = held_task(state, &lane.id());
            let (stopped, owner) =
                stop_at_job_boundary(&updated, held.map(|(_, owner)| owner), now);
            put_lane(tx, &stopped)?;
            if let (Some((task, _)), Some(owner)) = (held, owner) {
                put_owner(tx, task, &owner)?;
            }
            return Ok(());
        }
    }
    if updated != *lane {
        put_lane(tx, &updated)?;
    }
    Ok(())
}

fn pause(parts: &LanesParts, lane: &LaneId, by: &Author) -> Result<String> {
    parts.writer.transact(|tx| {
        let state = read_state_in(tx)?;
        let Some(record) = state.lanes.get(lane) else {
            bail!("{lane} is not a lane");
        };
        let mut updated = record.clone();
        if record.state == LaneState::Waiting {
            // The lane leaves the wait the ask was for, so the ask goes with the change.
            delete_ask(tx, lane)?;
        }
        let message = match record.state {
            LaneState::Idle | LaneState::Waiting if record.job.is_none() => {
                updated.state = LaneState::Paused;
                updated.pause_requested = false;
                updated.reason = Some(WaitReason::Other(format!("{PAUSED_BY}{by}")));
                format!("{lane} paused")
            }
            LaneState::Idle | LaneState::Waiting | LaneState::Running => {
                updated.pause_requested = true;
                updated.reason = Some(WaitReason::Other(format!("{PAUSE_REQUESTED}{by}")));
                format!("{lane} pauses after its job")
            }
            LaneState::Paused => format!("{lane} is already paused"),
            LaneState::Halted => format!("{lane} is halted; retry its run or adopt its task"),
            LaneState::Stopped => format!("{lane} is stopped"),
        };
        if updated != *record {
            put_lane(tx, &updated)?;
        }
        Ok(message)
    })
}

fn resume(parts: &LanesParts, lane: &LaneId) -> Result<String> {
    parts.writer.transact(|tx| {
        let state = read_state_in(tx)?;
        let Some(record) = state.lanes.get(lane) else {
            bail!("{lane} is not a lane");
        };
        let mut updated = record.clone();
        let message = match record.state {
            LaneState::Paused | LaneState::Stopped => {
                updated.state = LaneState::Idle;
                updated.reason = None;
                updated.pause_requested = false;
                updated.stop_requested = None;
                format!("{lane} resumed")
            }
            LaneState::Running if record.pause_requested => {
                updated.pause_requested = false;
                updated.reason = None;
                format!("{lane} no longer pauses")
            }
            LaneState::Running => format!("{lane} is already running"),
            // A lane waiting on a person: the ask is cleared and the lane evaluated again, by
            // the pass this commit starts (a lane holding a job waits for that job's hooks).
            LaneState::Waiting if ask_in(tx, lane)?.is_some_and(|ask| ask.applies_to(record)) => {
                delete_ask(tx, lane)?;
                if record.job.is_none() {
                    updated.state = LaneState::Idle;
                    updated.reason = None;
                    format!("{lane} resumed")
                } else {
                    format!("{lane} no longer waits on you; it still waits for its job's hooks")
                }
            }
            LaneState::Idle | LaneState::Waiting => format!("{lane} is not paused"),
            LaneState::Halted => bail!("{lane} is halted; retry its run or adopt its task"),
        };
        if updated != *record {
            put_lane(tx, &updated)?;
        }
        Ok(message)
    })
}

fn status(parts: &LanesParts) -> Result<LanesStatus> {
    let (state, record, asks) = parts
        .writer
        .transact(|tx| Ok((read_state_in(tx)?, dispatcher_record(tx)?, asks_in(tx)?)))?;
    Ok(status_of(&state, record, &asks))
}

/// The status as the store records it, for a process that reads the store beside the writer of
/// another (`bongos lanes status` with no lanes running). It shows the state at the last write.
pub fn status_from_read(read: &StateRead) -> Result<LanesStatus> {
    let state = read_state(read)?;
    let record = read_dispatcher_record(read)?;
    let asks = read_asks(read)?;
    Ok(status_of(&state, record, &asks))
}

fn status_of(
    state: &LanesState,
    record: Option<DispatcherRecord>,
    asks: &BTreeMap<LaneId, LaneAsk>,
) -> LanesStatus {
    let phase = record
        .as_ref()
        .map(|record| match (&record.ended, &record.stopped_by) {
            (Some(end), _) => DispatcherPhase::Ended(*end),
            (None, Some(_)) => DispatcherPhase::Pausing,
            (None, None) => DispatcherPhase::Running,
        });
    let (table_commit, table_wait) = record
        .map(|record| (record.table_commit, record.table_wait))
        .unwrap_or_default();
    let lanes = state
        .lanes
        .values()
        .map(|lane| LaneStatus {
            id: lane.id(),
            state: lane.state,
            reason: lane.reason.clone(),
            task: state.task_of(&lane.id()).cloned(),
            pause_requested: lane.pause_requested,
            stop_requested: lane.stop_requested.is_some(),
            asked: asked_text(state, asks, &lane.id()),
            job: lane.job.clone(),
            jobs_done: lane.jobs_done,
        })
        .collect();
    LanesStatus {
        phase,
        table_commit,
        table_wait,
        lanes,
    }
}

/// How the dispatcher's run ends with the lanes in `state`, or `None` while a lane is not at
/// rest. Cancelled when a stop of every lane was asked for; succeeded when the lanes that stopped
/// all stopped for a bound.
fn dispatcher_end(state: &LanesState, record: &DispatcherRecord) -> Option<(RunEnd, String)> {
    let at_rest = |lane: &LaneRecord| {
        matches!(
            lane.state,
            LaneState::Stopped | LaneState::Halted | LaneState::Paused
        )
    };
    if state.lanes.is_empty() || !state.lanes.values().all(at_rest) {
        return None;
    }
    if let Some(who) = &record.stopped_by {
        return Some((RunEnd::Cancelled, format!("stopped by {who}")));
    }
    let bounded =
        |lane: &LaneRecord| lane.reason == Some(WaitReason::Other(BOUND_REASON.to_string()));
    let mut stopped = state
        .lanes
        .values()
        .filter(|lane| lane.state == LaneState::Stopped)
        .peekable();
    if stopped.peek().is_some() && stopped.all(bounded) {
        return Some((
            RunEnd::Succeeded,
            "every lane reached its bound".to_string(),
        ));
    }
    None
}

/// Ends the dispatcher's run once, when every lane is at rest, and emits the event.
fn end_if_at_rest(parts: &LanesParts, events: &LaneEvents, cx: &mut App) -> Result<bool> {
    let decided = parts.writer.transact(|tx| {
        let Some(mut record) = dispatcher_record(tx)? else {
            return Ok(None);
        };
        if record.ended.is_some() {
            return Ok(None);
        }
        let state = read_state_in(tx)?;
        let Some((end, note)) = dispatcher_end(&state, &record) else {
            return Ok(None);
        };
        record.ended = Some(end);
        put_dispatcher(tx, &record)?;
        Ok(Some((record.run_id, end, note)))
    })?;
    let Some((run_id, end, note)) = decided else {
        return Ok(false);
    };
    parts.engine.end_dispatcher(&run_id, end, &note, cx);
    events.emit(LaneEvent::DispatcherEnded { run_id, end, note });
    Ok(true)
}

/// The reason of a lane whose requested pause has now been applied.
pub(crate) fn paused_reason(requested: &Option<WaitReason>) -> WaitReason {
    match requested {
        Some(WaitReason::Other(text)) => match text.strip_prefix(PAUSE_REQUESTED) {
            Some(who) => WaitReason::Other(format!("{PAUSED_BY}{who}")),
            None => WaitReason::Other("paused".to_string()),
        },
        _ => WaitReason::Other("paused".to_string()),
    }
}
