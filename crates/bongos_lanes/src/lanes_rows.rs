//! The rows the Lanes view shows, as data the view renders: the header ("3 of 5 lanes busy, 4
//! tasks ready, 6 waiting"), one row per lane with the reason when it is not working, and the
//! "needs you" listing. Reads only the lanes' status (run state and records) and, for the idle
//! reasons and the ready and waiting counts, a dry run, so a client window shows the same rows.

use std::collections::BTreeMap;

use bongos_lanes_model::{DryRun, LaneId, LaneState, TaskId};

use crate::dry_run::idle_reasons;
use crate::lane_control::{DispatcherPhase, LanesStatus};
use crate::wait_text::WaitTextFormatters;

/// What the rows need besides the status.
pub struct RowFacts<'a> {
    /// The goal of a task.
    pub goal: &'a dyn Fn(&TaskId) -> Option<String>,
    /// The lifecycle of a run (`running`, `halted`, ...).
    pub run_lifecycle: &'a dyn Fn(&str) -> Option<String>,
}

/// One lane's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRow {
    pub id: LaneId,
    pub kind: String,
    pub state: LaneState,
    pub task: Option<TaskId>,
    pub goal: Option<String>,
    pub workflow: Option<String>,
    pub run: Option<String>,
    pub run_lifecycle: Option<String>,
    /// When the lane's job started.
    pub job_started: Option<String>,
    /// Why the lane is idle, waiting, paused or stopped; `None` while it works.
    pub reason: Option<String>,
}

/// A lane waiting on a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsYouRow {
    pub lane: LaneId,
    pub task: Option<TaskId>,
    pub goal: Option<String>,
    pub text: String,
}

/// Everything the Lanes view lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanesViewRows {
    /// "3 of 5 lanes busy, 4 tasks ready, 6 waiting".
    pub header: String,
    /// The dispatcher run's state: `not started`, `running`, `pausing` or `ended`.
    pub dispatcher: &'static str,
    /// The commit of the lane table in force.
    pub table_commit: Option<String>,
    pub rows: Vec<LaneRow>,
    pub needs_you: Vec<NeedsYouRow>,
}

fn dispatcher_name(phase: Option<DispatcherPhase>) -> &'static str {
    match phase {
        None => "not started",
        Some(DispatcherPhase::Running) => "running",
        Some(DispatcherPhase::Pausing) => "pausing",
        Some(DispatcherPhase::Ended(_)) => "ended",
    }
}

/// The rows of the Lanes view from the lanes' `status`. `dry` (a dry run of the same moment)
/// gives the ready and waiting counts and the reason of an idle lane; without it the header
/// counts only busy lanes.
pub fn lanes_view_rows(
    status: &LanesStatus,
    facts: &RowFacts,
    dry: Option<&DryRun>,
    wait_text: &WaitTextFormatters,
) -> LanesViewRows {
    let busy = status
        .lanes
        .iter()
        .filter(|lane| lane.state == LaneState::Running)
        .count();
    let mut header = format!("{busy} of {} lanes busy", status.lanes.len());
    if let Some(dry) = dry {
        header.push_str(&format!(
            ", {} tasks ready, {} waiting",
            dry.ready,
            dry.waiting.len()
        ));
    }
    let idle: BTreeMap<LaneId, String> = dry
        .map(|dry| idle_reasons(dry, wait_text))
        .unwrap_or_default();
    let rows = status
        .lanes
        .iter()
        .map(|lane| {
            let reason = match lane.state {
                LaneState::Running => None,
                LaneState::Idle => idle.get(&lane.id).cloned(),
                _ => lane.reason.as_ref().map(|reason| wait_text.text(reason)),
            };
            LaneRow {
                id: lane.id.clone(),
                kind: lane.id.kind.clone(),
                state: lane.state,
                task: lane.task.clone(),
                goal: lane.task.as_ref().and_then(|task| (facts.goal)(task)),
                workflow: lane.job.as_ref().map(|job| job.workflow.clone()),
                run: lane.job.as_ref().map(|job| job.run_id.clone()),
                run_lifecycle: lane
                    .job
                    .as_ref()
                    .and_then(|job| (facts.run_lifecycle)(&job.run_id)),
                job_started: lane.job.as_ref().map(|job| job.started.clone()),
                reason,
            }
        })
        .collect();
    let needs_you = status
        .lanes
        .iter()
        .filter_map(|lane| {
            lane.asked.as_ref().map(|text| NeedsYouRow {
                lane: lane.id.clone(),
                task: lane.task.clone(),
                goal: lane.task.as_ref().and_then(|task| (facts.goal)(task)),
                text: text.clone(),
            })
        })
        .collect();
    LanesViewRows {
        header,
        dispatcher: dispatcher_name(status.phase),
        table_commit: status.table_commit.clone(),
        rows,
        needs_you,
    }
}
