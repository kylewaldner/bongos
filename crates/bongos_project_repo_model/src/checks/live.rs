//! "Started": which tasks have work that a definition change must not disturb, read from
//! the state store at check time and from the registered live sources (active runs, claims, lane
//! ownership, queue claims, a move's `moving` claims). Never read from the repository.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use bongos_work_model::TodoState;

use crate::split::TaskStateRecord;

/// Why a task is held: only a move holds tasks today.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldReason {
    Moving(MoveHold),
}

/// A `moving` claim of a move in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveHold {
    pub move_id: String,
    /// "moving to <project>" or "moving from <project>".
    pub label: String,
    /// The change ids the move's coordinator created and recorded in its `moves` rows (the source
    /// removal, a rollback). Only these may touch the held task.
    pub coordinator_changes: BTreeSet<String>,
}

impl HoldReason {
    /// Whether the change `change_id` is one the hold's own coordinator made.
    pub fn exempts(&self, change_id: &str) -> bool {
        match self {
            HoldReason::Moving(hold) => hold.coordinator_changes.contains(change_id),
        }
    }

    pub fn move_id(&self) -> &str {
        match self {
            HoldReason::Moving(hold) => &hold.move_id,
        }
    }
}

impl fmt::Display for HoldReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HoldReason::Moving(hold) => write!(f, "{} (move {})", hold.label, hold.move_id),
        }
    }
}

/// Something besides the task's state row that makes tasks started or holds them: an active run,
/// claims and lane ownership, queue claims, or the `moving` claims of a move in progress.
pub trait LiveSource: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn started(&self, task_id: &str) -> bool;
    fn held(&self, _task_id: &str) -> Option<HoldReason> {
        None
    }
}

/// The live facts a landing check reads: the task state rows and the registered sources.
#[derive(Clone, Default)]
pub struct LiveFacts {
    states: BTreeMap<String, TaskStateRecord>,
    sources: Vec<Arc<dyn LiveSource>>,
}

impl fmt::Debug for LiveFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveFacts")
            .field("states", &self.states)
            .field(
                "sources",
                &self.sources.iter().map(|s| s.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl LiveFacts {
    /// Facts over the state rows `states`, by task id, with no source registered.
    pub fn from_states(states: BTreeMap<String, TaskStateRecord>) -> Self {
        Self {
            states,
            sources: Vec::new(),
        }
    }

    pub fn register(&mut self, source: Arc<dyn LiveSource>) {
        self.sources.push(source);
    }

    /// Whether the task has work: its state row is started ([`state_is_started`]), a source says
    /// so, or a source holds it.
    pub fn started(&self, task_id: &str) -> bool {
        self.states.get(task_id).is_some_and(state_is_started)
            || self.sources.iter().any(|source| source.started(task_id))
            || self.held(task_id).is_some()
    }

    /// The first hold a source reports on the task.
    pub fn held(&self, task_id: &str) -> Option<HoldReason> {
        self.sources.iter().find_map(|source| source.held(task_id))
    }

    /// The started tasks among `ids`; the landing's queue step compares this set with the one a
    /// check saw and reruns the check on a difference.
    pub fn started_set<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
        ids.into_iter()
            .filter(|id| self.started(id))
            .map(str::to_string)
            .collect()
    }

    /// The ids of the task's todos that have state (not open, or started).
    pub fn todos_with_state(&self, task_id: &str) -> BTreeSet<String> {
        self.states
            .get(task_id)
            .map(|record| {
                record
                    .todos
                    .iter()
                    .filter(|todo| todo.state != TodoState::Open || todo.started.is_some())
                    .filter_map(|todo| todo.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Whether a state row marks its task started: any todo not open, any todo started, a branch, a
/// worktree or a result.
pub fn state_is_started(record: &TaskStateRecord) -> bool {
    record
        .todos
        .iter()
        .any(|todo| todo.state != TodoState::Open || todo.started.is_some())
        || record.branch.is_some()
        || record.worktree.is_some()
        || record.result.is_some()
}
