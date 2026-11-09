//! The only two statuses bongos ever writes to Linear: started when a plan begins (D13) and
//! canceled when Kyle drops a task and says yes (D14). Everything here decides; nothing sends.
//! No code path leads from a result, a close or a merge to a status write, and never to
//! completed.

use std::fmt;

use bongos_work_model::commands::{Change, Context, Outcome};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::{self, Place};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{LinearCancel, Status, TaskFile, TaskId, TodoRole};

use crate::api::{StateKind, WorkflowState};
use crate::sync::{edit_linked, link_of};

/// A move of one ticket to one workflow state.
#[derive(Clone, Debug, PartialEq)]
pub struct StateWrite {
    pub task_id: TaskId,
    pub issue_id: String,
    pub state_id: String,
    pub state_name: String,
}

impl StateWrite {
    fn new(task_id: &str, issue_id: &str, state: &WorkflowState) -> Self {
        Self {
            task_id: task_id.to_string(),
            issue_id: issue_id.to_string(),
            state_id: state.id.clone(),
            state_name: state.name.clone(),
        }
    }
}

/// Why `started_sent` is recorded with no write to Linear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoWrite {
    /// A stacked leaf never moves its ticket (its PR targets the parent's branch).
    Stacked,
    /// The ticket is already past backlog, unstarted and triage, or not in a state bongos moves
    /// from; it never moves backwards.
    State(String),
    /// The team has no state of kind `started`.
    NoStartedState,
}

impl NoWrite {
    /// The history detail of the record.
    pub fn detail(&self) -> String {
        match self {
            NoWrite::Stacked => "no Linear status for a stacked leaf".to_string(),
            NoWrite::State(state) => format!("ticket is {state}; not moved"),
            NoWrite::NoStartedState => "the team has no started state".to_string(),
        }
    }
}

/// What to do about a linked task whose plan may have begun.
#[derive(Clone, Debug, PartialEq)]
pub enum StartedDecision {
    /// Nothing: the task is not linked, `started_sent` is already there, or the plan has not
    /// begun.
    Nothing,
    /// Move the ticket, then record `started_sent` with [`record_started`].
    Write(StateWrite),
    /// Write nothing to Linear, but record `started_sent` so it is not reconsidered.
    Record(NoWrite),
}

/// Whether a leaf's plan has begun: its `plans` todo has `started`, or, with no `plans` todo, its
/// first todo does.
pub fn plan_begun(file: &TaskFile) -> bool {
    let todo = file
        .todos
        .iter()
        .find(|todo| todo.role == Some(TodoRole::Plans))
        .or_else(|| file.todos.first());
    todo.is_some_and(|todo| todo.started.is_some())
}

/// D13 for the task `task_id` against its ticket's state and its team's states.
///
/// Only once (`started_sent` absent) and only for a synced link. A leaf: once its plan has
/// begun; never when stacked. From `backlog`, `unstarted` or `triage` only, to the team's
/// `started` state with the lowest `position`; from any other state nothing is written and the
/// decision is recorded. A group: the same, once a linked leaf under it that is not stacked has
/// begun its plan.
pub fn started_decision(
    tree: &WorkTree,
    task_id: &str,
    ticket_state: &WorkflowState,
    team_states: &[WorkflowState],
) -> StartedDecision {
    let Ok(place) = Place::locate(tree, task_id) else {
        return StartedDecision::Nothing;
    };
    let task = place.task;
    let (Some(file), Some((_, state))) = (&task.file, link_of(task)) else {
        return StartedDecision::Nothing;
    };
    if state.started_sent.is_some() {
        return StartedDecision::Nothing;
    }
    if derive::shape(task, tree) == Shape::Group {
        let begun = task.walk().into_iter().skip(1).any(|leaf| {
            leaf.children.is_empty()
                && link_of(leaf).is_some()
                && leaf
                    .file
                    .as_ref()
                    .is_some_and(|file| file.stacked_on.is_none() && plan_begun(file))
        });
        if !begun {
            return StartedDecision::Nothing;
        }
    } else {
        if !plan_begun(file) {
            return StartedDecision::Nothing;
        }
        if file.stacked_on.is_some() {
            return StartedDecision::Record(NoWrite::Stacked);
        }
    }
    if !matches!(
        ticket_state.kind,
        StateKind::Backlog | StateKind::Unstarted | StateKind::Triage
    ) {
        return StartedDecision::Record(NoWrite::State(ticket_state.name.clone()));
    }
    match lowest(team_states, &StateKind::Started) {
        Some(target) => StartedDecision::Write(StateWrite::new(&file.id, &state.issue_id, target)),
        None => StartedDecision::Record(NoWrite::NoStartedState),
    }
}

/// [`started_decision`] when only a write matters.
pub fn started_write(
    tree: &WorkTree,
    task_id: &str,
    ticket_state: &WorkflowState,
    team_states: &[WorkflowState],
) -> Option<StateWrite> {
    match started_decision(tree, task_id, ticket_state, team_states) {
        StartedDecision::Write(write) => Some(write),
        StartedDecision::Nothing | StartedDecision::Record(_) => None,
    }
}

/// The state of `kind` with the lowest position.
fn lowest<'a>(states: &'a [WorkflowState], kind: &StateKind) -> Option<&'a WorkflowState> {
    states
        .iter()
        .filter(|state| state.kind == *kind)
        .min_by(|a, b| a.position.total_cmp(&b.position))
}

/// Records `started_sent` on a linked task: after the Linear write succeeded (`reason` is
/// `None`), or when none is needed (`reason` says why, as history detail). Refused when it is
/// already recorded.
pub fn record_started(
    tree: &WorkTree,
    ctx: &Context,
    task_id: &str,
    reason: Option<&NoWrite>,
) -> Outcome {
    edit_linked(tree, ctx, task_id, |place, _, state, _| {
        if state.started_sent.is_some() {
            return Err(format!(
                "{} already recorded its started write",
                place.slug()
            ));
        }
        state.started_sent = Some(ctx.at);
        Ok(("linear started".to_string(), reason.map(NoWrite::detail)))
    })
}

/// Why no cancel question is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoQuestion {
    NotLinked,
    NotCancelled,
    /// The answer was recorded: it is asked once.
    Answered,
    /// A stacked leaf: bongos writes no Linear status for it.
    Stacked(String),
    /// A group with an open leaf under it never cancels its ticket.
    OpenLeaf(String),
}

impl fmt::Display for NoQuestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoQuestion::NotLinked => write!(f, "the task is not linked to Linear"),
            NoQuestion::NotCancelled => write!(f, "the task is not cancelled"),
            NoQuestion::Answered => write!(f, "the cancel question was already answered"),
            NoQuestion::Stacked(identifier) => write!(
                f,
                "{identifier} is stacked; bongos writes no Linear status for a stacked leaf"
            ),
            NoQuestion::OpenLeaf(slug) => {
                write!(f, "{slug} is still open; no ticket is cancelled yet")
            }
        }
    }
}

/// A ticket the cancel question may move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelTarget {
    pub task_id: TaskId,
    pub identifier: String,
    pub issue_id: String,
}

/// The question to ask when a linked task turns `cancelled`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelQuestion {
    /// "Cancel SEQ-121 in Linear?": Cancel ticket (default) / Leave it.
    Leaf(CancelTarget),
    /// "Cancel SEQ-120 and its 3 subtickets?": Leave all (default) / Cancel leaves only /
    /// Cancel all.
    Group {
        group: CancelTarget,
        leaves: Vec<CancelTarget>,
    },
}

/// Kyle's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelAnswer {
    /// A leaf: cancel the ticket.
    Cancel,
    /// Leave the ticket(s); the question is not asked again.
    Leave,
    /// A group: cancel its leaves' tickets only.
    LeavesOnly,
    /// A group: cancel the group's ticket and its leaves'.
    All,
}

fn target(task: &Task) -> Option<CancelTarget> {
    let (identifier, state) = link_of(task)?;
    Some(CancelTarget {
        task_id: task.id()?.to_string(),
        identifier: identifier.to_string(),
        issue_id: state.issue_id.clone(),
    })
}

fn cancelled(task: &Task) -> bool {
    task.file
        .as_ref()
        .is_some_and(|file| file.status == Status::Cancelled)
}

/// Refuses while any leaf under the group `task_id` is open, whatever the answer. `Ok` for a
/// group with no open leaf. A leaf is open while it is not done and not cancelled.
pub fn may_cancel_group(tree: &WorkTree, task_id: &str) -> Result<(), NoQuestion> {
    let place = Place::locate(tree, task_id).map_err(|_| NoQuestion::NotLinked)?;
    match rules::open_descendants(place.task, tree)
        .into_iter()
        .find(|task| derive::shape(task, tree) == Shape::Leaf)
    {
        Some(open) => Err(NoQuestion::OpenLeaf(open.slug.clone())),
        None => Ok(()),
    }
}

/// D14: the question a linked task that turned `cancelled` raises, or why there is none.
///
/// A leaf: "Cancel SEQ-121 in Linear?". Never for a stacked leaf. A group: only when it is
/// cancelled itself and no leaf under it is open; it offers its linked, cancelled, unstacked
/// leaves and itself. Asked once: a recorded `cancel` ends it.
pub fn cancel_question(tree: &WorkTree, task_id: &str) -> Result<CancelQuestion, NoQuestion> {
    let place = Place::locate(tree, task_id).map_err(|_| NoQuestion::NotLinked)?;
    let task = place.task;
    let (Some(file), Some((identifier, state))) = (&task.file, link_of(task)) else {
        return Err(NoQuestion::NotLinked);
    };
    if file.status != Status::Cancelled {
        return Err(NoQuestion::NotCancelled);
    }
    if state.cancel.is_some() {
        return Err(NoQuestion::Answered);
    }
    let Some(this) = target(task) else {
        return Err(NoQuestion::NotLinked);
    };
    if derive::shape(task, tree) == Shape::Leaf {
        if file.stacked_on.is_some() {
            return Err(NoQuestion::Stacked(identifier.to_string()));
        }
        return Ok(CancelQuestion::Leaf(this));
    }
    may_cancel_group(tree, task_id)?;
    let leaves = task
        .walk()
        .into_iter()
        .skip(1)
        .filter(|leaf| leaf.children.is_empty() && cancelled(leaf))
        .filter(|leaf| {
            leaf.file.as_ref().is_some_and(|file| {
                file.stacked_on.is_none()
                    && file
                        .linear_state
                        .as_ref()
                        .is_some_and(|state| state.cancel.is_none())
            })
        })
        .filter_map(target)
        .collect();
    Ok(CancelQuestion::Group {
        group: this,
        leaves,
    })
}

/// `status::cancel_write`: the move to the team's `canceled` state with the lowest position, only
/// for a cancelled linked task, from a state that is not completed, canceled or duplicate, and
/// never for a stacked leaf. Duplicate and unknown kinds are never targets.
pub fn cancel_write(
    tree: &WorkTree,
    task_id: &str,
    ticket_state: &WorkflowState,
    team_states: &[WorkflowState],
) -> Option<StateWrite> {
    let place = Place::locate(tree, task_id).ok()?;
    let task = place.task;
    let file = task.file.as_ref()?;
    let (_, state) = link_of(task)?;
    if file.status != Status::Cancelled || file.stacked_on.is_some() {
        return None;
    }
    if matches!(ticket_state.kind, StateKind::Completed) || ticket_state.kind.is_dropped() {
        return None;
    }
    let target = lowest(team_states, &StateKind::Canceled)?;
    Some(StateWrite::new(&file.id, &state.issue_id, target))
}

/// What Kyle's answer makes happen.
#[derive(Clone, Debug, PartialEq)]
pub struct CancelPlan {
    /// The tickets to move (each still goes through [`cancel_write`]'s rules).
    pub writes: Vec<StateWrite>,
    /// The answer to record per task: `cancelled` once its write succeeded (record it after the
    /// write), `declined` at once.
    pub cancelled: Vec<TaskId>,
    pub declined: Vec<TaskId>,
}

/// Turns an answer into the writes and records. `ticket_state` gives a ticket's current state by
/// its UUID, `team_states` the states of the team of each ticket's team id. A ticket whose state
/// is unknown or not cancelable gets no write and no `cancelled` record.
pub fn cancel_plan(
    tree: &WorkTree,
    question: &CancelQuestion,
    answer: CancelAnswer,
    ticket_state: &dyn Fn(&str) -> Option<WorkflowState>,
    team_states: &dyn Fn(&str) -> Vec<WorkflowState>,
) -> CancelPlan {
    let (all, cancel_group, cancel_leaves): (Vec<&CancelTarget>, bool, bool) = match question {
        CancelQuestion::Leaf(leaf) => (vec![leaf], false, answer == CancelAnswer::Cancel),
        CancelQuestion::Group { group, leaves } => {
            let mut all: Vec<&CancelTarget> = leaves.iter().collect();
            all.push(group);
            (
                all,
                answer == CancelAnswer::All,
                matches!(answer, CancelAnswer::All | CancelAnswer::LeavesOnly),
            )
        }
    };
    let group_id = match question {
        CancelQuestion::Group { group, .. } => Some(group.task_id.as_str()),
        CancelQuestion::Leaf(_) => None,
    };
    let mut plan = CancelPlan {
        writes: Vec::new(),
        cancelled: Vec::new(),
        declined: Vec::new(),
    };
    for each in all {
        let is_group = group_id == Some(each.task_id.as_str());
        let wanted = if is_group {
            cancel_group
        } else {
            cancel_leaves
        };
        if !wanted {
            plan.declined.push(each.task_id.clone());
            continue;
        }
        let write = ticket_state(&each.issue_id).and_then(|state| {
            let team = tree
                .task_by_id(&each.task_id)
                .and_then(link_of)
                .map(|(_, linked)| linked.team_id.clone())
                .unwrap_or_default();
            cancel_write(tree, &each.task_id, &state, &team_states(&team))
        });
        if let Some(write) = write {
            plan.writes.push(write);
            plan.cancelled.push(each.task_id.clone());
        }
    }
    plan
}

/// Records the answer on a linked task as `linear_state.cancel`, so the question is asked once.
pub fn record_cancel(
    tree: &WorkTree,
    ctx: &Context,
    task_id: &str,
    answer: LinearCancel,
) -> Outcome {
    edit_linked(tree, ctx, task_id, |_, _, state, _| {
        state.cancel = Some(answer);
        let event = match answer {
            LinearCancel::Cancelled => "linear cancelled",
            LinearCancel::Declined => "linear cancel declined",
        };
        Ok((event.to_string(), None))
    })
}

/// `task: reopen` clears the answer and writes nothing to Linear (there is no reopen).
pub fn clear_cancel(tree: &WorkTree, ctx: &Context, task_id: &str) -> Option<Change> {
    let task = tree.task_by_id(task_id)?;
    link_of(task)?.1.cancel?;
    edit_linked(tree, ctx, task_id, |_, _, state, _| {
        state.cancel = None;
        Ok(("linear cancel cleared".to_string(), None))
    })
    .ok()
}
