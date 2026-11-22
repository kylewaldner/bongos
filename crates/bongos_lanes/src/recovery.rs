//! Recovery after a crash, when a process becomes a project's writer and on every `lanes: start`.
//!
//! The run engine's load comes first: it kills the orphaned node processes and marks each run
//! that had a node in flight interrupted (waiting on you), and judges a run whose log ends with
//! `HandedOver` by the events before it. Recovery then reconciles the lanes state with the runs:
//!
//! - A dispatcher record whose host is dead is closed: its run is interrupted, then cancelled
//!   with [`HOST_STOPPED`]. `lanes: start` then starts a new dispatcher run; the lanes keep their
//!   tasks, and a halted or paused lane stays so.
//! - A lane job whose run never started (a crash between the pick and the start) is cleared and
//!   the lane is idle again, its owner record kept; an extension job's extensions hear
//!   `JobChange::NotStarted` in the same transaction.
//! - A lane run an interruption stopped stays on its running lane. With `on_interruption = ask`
//!   it waits on a person; with `resume` it is resumed once: `interruptions/<run id>` records the
//!   resume in a transaction committed before the resume, so no run is ever resumed twice by
//!   recovery, and a second interruption waits.
//!
//! Every step is idempotent: running recovery again changes nothing more.

use std::rc::Rc;

use anyhow::{Context as _, Result};
use bongos_lanes_model::{LaneId, LaneState, LaneTable};
use bongos_project_repo::{StateRead, StateTx};
use gpui::{App, Task};
use serde::{Deserialize, Serialize};

use crate::dispatcher::{
    DispatcherRecord, JobEngine, LanesParts, clear_unstarted, dispatcher_record, put_dispatcher,
};
use crate::lane_control::RunEnd;
use crate::pick_extension::{PickExtensions, with_engine};
use crate::settings::LanesOnInterruption;
use crate::store::{LANES_NAMESPACE, read_state};

/// The note a dead host's dispatcher run is cancelled with.
pub const HOST_STOPPED: &str = "host stopped";
/// The key prefix of the interruption records, followed by the run id.
pub const INTERRUPTIONS_PREFIX: &str = "interruptions/";

/// What recovery did to one run an interruption stopped, under `interruptions/<run id>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptionRecord {
    /// How many times recovery resumed the run: at most one.
    pub resumed: u32,
}

fn interruption_key(run: &str) -> String {
    format!("{INTERRUPTIONS_PREFIX}{run}")
}

/// The interruption record of `run` in a read of the store.
pub fn read_interruption(read: &StateRead, run: &str) -> Result<Option<InterruptionRecord>> {
    read.kv(LANES_NAMESPACE)
        .get(&interruption_key(run))?
        .map(|value| value.try_into().context("reading an interruption record"))
        .transpose()
}

fn interruption_in(tx: &mut StateTx, run: &str) -> Result<Option<InterruptionRecord>> {
    tx.kv(LANES_NAMESPACE)
        .get(&interruption_key(run))?
        .map(|value| value.try_into().context("reading an interruption record"))
        .transpose()
}

fn put_interruption(tx: &mut StateTx, run: &str, record: &InterruptionRecord) -> Result<()> {
    let value = toml::Value::try_from(record).context("serializing an interruption record")?;
    tx.kv(LANES_NAMESPACE).put(&interruption_key(run), value)
}

/// What recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovered {
    /// The dispatcher run of a dead host that was closed.
    pub closed_dispatcher: Option<String>,
    /// The lanes whose job never started and was cleared.
    pub cleared: Vec<LaneId>,
    /// The interrupted lane runs resumed now.
    pub resumed: Vec<String>,
    /// The interrupted lane runs left waiting on a person.
    pub waiting: Vec<String>,
}

/// The interruption policy of a project: `lanes.toml`'s `on_interruption` when it names one,
/// else the user setting. An unknown value in the table is logged and the setting applies.
pub fn interruption_policy(
    table: Option<&LaneTable>,
    setting: LanesOnInterruption,
) -> LanesOnInterruption {
    match table.and_then(|table| table.on_interruption.as_deref()) {
        Some("ask") => LanesOnInterruption::Ask,
        Some("resume") => LanesOnInterruption::Resume,
        Some(other) => {
            log::warn!(
                "lanes: on_interruption = {other:?} in lanes.toml is neither ask nor resume; \
                 the user setting applies"
            );
            setting
        }
        None => setting,
    }
}

/// The user setting `lanes.on_interruption`; `ask` where the settings are not registered.
pub(crate) fn setting(cx: &App) -> LanesOnInterruption {
    use ::settings::Settings as _;
    crate::LanesSettings::try_get(cx)
        .map(|settings| settings.on_interruption)
        .unwrap_or_default()
}

/// Whether the dispatcher `record` belongs to a live host other than a finished run: while it
/// does, that host may have a job start in flight, so no lane job is taken for unstarted.
fn live_dispatcher(parts: &LanesParts, record: Option<&DispatcherRecord>) -> bool {
    record.is_some_and(|record| record.ended.is_none() && parts.host.host_live(&record.host))
}

/// Recovery when this process became the project's writer, after the run engine loaded the runs:
/// closes a dead host's dispatcher, clears the lane jobs that never started and applies `policy`
/// to the interrupted lane runs.
pub fn recover(parts: &LanesParts, policy: LanesOnInterruption, cx: &mut App) -> Result<Recovered> {
    let closed_dispatcher = close_dead_dispatcher(parts, cx)?;
    let cleared = clear_unstarted_jobs(parts, cx)?;
    let (resumed, waiting) = apply_interruption_policy(parts, policy, cx)?;
    Ok(Recovered {
        closed_dispatcher,
        cleared,
        resumed,
        waiting,
    })
}

/// Closes the dispatcher record of a dead host, once: the record is marked ended (cancelled) in a
/// transaction, then its run is interrupted and cancelled with [`HOST_STOPPED`]. `None` when there
/// is no record, it ended already, or its host lives.
pub(crate) fn close_dead_dispatcher(parts: &LanesParts, cx: &mut App) -> Result<Option<String>> {
    let host = parts.host.clone();
    let closed = parts.writer.transact(|tx| {
        let Some(mut record) = dispatcher_record(tx)? else {
            return Ok(None);
        };
        if record.ended.is_some() || host.host_live(&record.host) {
            return Ok(None);
        }
        record.ended = Some(RunEnd::Cancelled);
        put_dispatcher(tx, &record)?;
        Ok(Some((record.run_id, record.host)))
    })?;
    let Some((run, dead)) = closed else {
        return Ok(None);
    };
    log::info!("lanes: the dispatcher host {dead} is gone; closing its run {run}");
    let engine = &parts.engine;
    if engine
        .run_status(&run)
        .is_some_and(|status| status.is_active())
    {
        engine.interrupt_run(&run, cx);
        engine.end_dispatcher(&run, RunEnd::Cancelled, HOST_STOPPED, cx);
    }
    Ok(Some(run))
}

/// Clears the lane jobs whose run never started, unless a live host runs the dispatcher (its
/// start may be in flight). The lanes cleared.
pub(crate) fn clear_unstarted_jobs(parts: &LanesParts, cx: &mut App) -> Result<Vec<LaneId>> {
    let extensions = PickExtensions::extensions(cx);
    let now = parts.host.now();
    let app: &App = cx;
    with_engine(&parts.engine, || {
        parts.writer.transact(|tx| {
            if live_dispatcher(parts, dispatcher_record(tx)?.as_ref()) {
                return Ok(Vec::new());
            }
            clear_unstarted(tx, &*parts.engine, &extensions, &now, app)
        })
    })
}

/// Applies `policy` to every lane run an interruption stopped (a halted or paused lane is left as
/// it is). `ask` leaves them waiting. `resume` resumes each run once: its interruption record is
/// written in a transaction committed before the resume, and a run resumed once already waits.
/// The runs resumed and the runs left waiting.
pub(crate) fn apply_interruption_policy(
    parts: &LanesParts,
    policy: LanesOnInterruption,
    cx: &mut App,
) -> Result<(Vec<String>, Vec<String>)> {
    let engine: &Rc<dyn JobEngine> = &parts.engine;
    let state = parts.writer.read(read_state)?;
    let mut resumed = Vec::new();
    let mut waiting = Vec::new();
    for lane in state.lanes.values() {
        if matches!(lane.state, LaneState::Halted | LaneState::Paused) {
            continue;
        }
        let Some(job) = &lane.job else {
            continue;
        };
        let run = job.run_id.clone();
        if !engine.interrupted(&run) {
            continue;
        }
        if policy == LanesOnInterruption::Ask {
            waiting.push(run);
            continue;
        }
        let take = parts.writer.transact(|tx| {
            let mut record = interruption_in(tx, &run)?.unwrap_or_default();
            if record.resumed >= 1 {
                return Ok(false);
            }
            record.resumed += 1;
            put_interruption(tx, &run, &record)?;
            Ok(true)
        })?;
        if !take {
            log::info!("lanes: run {run} was interrupted again after its one resume; it waits");
            waiting.push(run);
            continue;
        }
        log::info!("lanes: resuming the interrupted run {run} of {}", lane.id());
        match engine.resume_run(&run, cx) {
            Ok(()) => resumed.push(run),
            Err(reason) => {
                // Recorded as resumed all the same: it is never resumed twice on its own.
                log::error!("lanes: resuming the interrupted run {run}: {reason}");
                waiting.push(run);
            }
        }
    }
    Ok((resumed, waiting))
}

/// How often recovery at writer start looks again while the runs are not loaded yet.
pub const RECOVERY_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
/// How many times it looks before it gives up (logged).
const RECOVERY_TRIES: usize = 600;

/// Whether recovery at writer start could run now.
enum Attempt {
    Done,
    NotYet,
    Never(String),
}

/// The writer participant that runs recovery when this process becomes a project's writer (a
/// window that opened it first, an engine host that took the lock, or a take-back), once the run
/// engine has loaded the runs. It has nothing to drain.
pub(crate) struct LanesRecovery;

impl bongos_engine_host::WriterParticipant for LanesRecovery {
    fn started(&self, project: &str, cx: &mut App) {
        use ::settings::Settings as _;
        if !crate::LanesSettings::try_get(cx).is_some_and(|settings| settings.enabled) {
            return;
        }
        let project = project.to_string();
        cx.spawn(async move |cx| {
            for _ in 0..RECOVERY_TRIES {
                let attempt = cx.update(|cx| attempt(&project, cx));
                let attempt = match attempt {
                    AttemptTask::Ready(attempt) => attempt,
                    AttemptTask::Wait(task) => task.await,
                };
                match attempt {
                    Attempt::Done => return,
                    Attempt::Never(reason) => {
                        log::info!("lanes: no recovery of {project}: {reason}");
                        return;
                    }
                    Attempt::NotYet => {}
                }
                cx.background_executor().timer(RECOVERY_RETRY).await;
            }
            log::error!("lanes: the runs of {project} were never loaded; no recovery ran");
        })
        .detach();
    }

    fn drain(&self, _project: &str, _cx: &mut App) -> Task<()> {
        Task::ready(())
    }

    fn undrain(&self, _project: &str, _cx: &mut App) -> Result<()> {
        Ok(())
    }
}

enum AttemptTask {
    Ready(Attempt),
    Wait(Task<Attempt>),
}

/// One try of recovery at writer start for `project`.
fn attempt(project: &str, cx: &mut App) -> AttemptTask {
    use bongos_workflow::WorkflowEngine;
    let Some(engine) = WorkflowEngine::ensure(cx) else {
        return AttemptTask::Ready(Attempt::Never("workflows are off".to_string()));
    };
    if !engine.runs_loaded(cx) {
        return AttemptTask::Ready(Attempt::NotYet);
    }
    let Some(lanes) = crate::host::ensure_window_lanes(cx) else {
        return AttemptTask::Ready(Attempt::NotYet);
    };
    if lanes.parts.host.project_id() != project {
        return AttemptTask::Ready(Attempt::Never(
            "this process holds the writer of another project".to_string(),
        ));
    }
    let parts = lanes.parts.clone();
    let verified = parts
        .writer
        .read(|read| read.verified_commit())
        .ok()
        .flatten();
    let loading = verified
        .as_ref()
        .map(|commit| parts.host.load_commit(commit, cx));
    let project = project.to_string();
    AttemptTask::Wait(cx.spawn(async move |cx| {
        if let Some(loading) = loading {
            loading.await;
        }
        cx.update(|cx| {
            let table = verified
                .as_deref()
                .and_then(|commit| parts.host.lane_table(commit).ok());
            let policy = interruption_policy(table.as_ref(), setting(cx));
            match recover(&parts, policy, cx) {
                Ok(recovered) => {
                    if recovered != Recovered::default() {
                        log::info!("lanes: recovered {project}: {recovered:?}");
                    }
                }
                Err(error) => log::error!("lanes: recovering {project}: {error:#}"),
            }
            Attempt::Done
        })
    }))
}
