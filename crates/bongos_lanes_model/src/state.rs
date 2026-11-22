//! Run state for lanes: owner and lane records, and the pure transitions between them.
//!
//! Nothing here touches a store or the clock. Callers pass timestamps in as plain strings and
//! apply the returned records themselves.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::stamp::StampDiff;
use crate::table::Pin;

/// A lane instance such as `build-2`. Indexes are 1-based.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LaneId {
    pub kind: String,
    pub index: u32,
}

impl LaneId {
    pub fn new(kind: impl Into<String>, index: u32) -> Self {
        Self {
            kind: kind.into(),
            index,
        }
    }

    /// Parses `<kind>-<index>`.
    pub fn parse(text: &str) -> Option<Self> {
        let (kind, index) = text.rsplit_once('-')?;
        if kind.is_empty() {
            return None;
        }
        let index: u32 = index.parse().ok()?;
        (index >= 1).then(|| Self::new(kind, index))
    }
}

impl fmt::Display for LaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.kind, self.index)
    }
}

impl Serialize for LaneId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for LaneId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        LaneId::parse(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("not a lane id: {text:?}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    Person(String),
    Lane(LaneId),
    System,
}

impl fmt::Display for Author {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Author::Person(name) => f.write_str(name),
            Author::Lane(lane) => write!(f, "{lane}"),
            Author::System => f.write_str("system"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopCause {
    /// The owner asked the lanes to stop.
    Stop,
    /// The lane count was lowered.
    Count,
    Rollout,
    Bound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Dependency {
        on: TaskId,
        #[serde(default)]
        progress: String,
    },
    Area {
        tag: String,
        with: TaskId,
        #[serde(rename = "where", default)]
        where_: String,
    },
    Owner {
        lane: LaneId,
        state: OwnerState,
    },
    Pin {
        to: String,
    },
    /// A pin sends the task to a lane that is not running.
    PinNotRunning {
        to: String,
    },
    /// An ahead kind's cap has no room: `n` tasks are prepared and not started, or being
    /// prepared.
    AheadCap {
        n: u32,
        cap: u32,
    },
    /// The structure of the graph differs from the recorded stamp.
    StaleGraph {
        diff: StampDiff,
    },
    /// The workflow's instance limit has no room.
    InstanceLimit {
        workflow: String,
        active: u32,
        limit: u32,
        /// One run holding an instance, when known.
        #[serde(default)]
        run: Option<String>,
    },
    /// No dispatch rule of the lane's kind gives a workflow, and the open todo names none.
    NoWorkflow,
    /// Another lane kind's rules give this task a workflow; this one's do not.
    LaneKind {
        kind: String,
    },
    /// The leaf already has an active run.
    ActiveRun {
        run: String,
    },
    Held {
        reason: String,
    },
    /// The task is outside the dispatcher's bound.
    Bound,
    /// The task is already done.
    Finished,
    Environment(String),
    Failed(String),
    Other(String),
}

impl fmt::Display for WaitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitReason::Dependency { on, progress } if progress.is_empty() => {
                write!(f, "waits on {on}")
            }
            WaitReason::Dependency { on, progress } => write!(f, "waits on {on}: {progress}"),
            WaitReason::Area { tag, with, where_ } if where_.is_empty() => {
                write!(f, "area {tag} shared with {with}")
            }
            WaitReason::Area { tag, with, where_ } => {
                write!(f, "area {tag} shared with {with}, {where_}")
            }
            WaitReason::Owner { lane, state } => write!(f, "owned by {lane} ({state})"),
            WaitReason::Pin { to } => write!(f, "pinned to {to}"),
            WaitReason::PinNotRunning { to } => write!(f, "pinned to {to}, which is not running"),
            WaitReason::AheadCap { n, cap } => {
                write!(
                    f,
                    "ahead cap reached: {n} prepared or preparing (cap {cap})"
                )
            }
            WaitReason::StaleGraph { diff } => write!(f, "{diff}"),
            WaitReason::InstanceLimit {
                workflow,
                active,
                limit,
                run,
            } => {
                write!(f, "workflow {workflow} at its limit ({active} of {limit})")?;
                if let Some(run) = run {
                    write!(f, ", run {}", short_run(run))?;
                }
                Ok(())
            }
            WaitReason::NoWorkflow => f.write_str("no workflow"),
            WaitReason::LaneKind { kind } => write!(f, "only a {kind} lane takes it"),
            WaitReason::ActiveRun { run } => write!(f, "run {} is active", short_run(run)),
            WaitReason::Held { reason } => write!(f, "held: {reason}"),
            WaitReason::Bound => f.write_str("outside the bound"),
            WaitReason::Finished => f.write_str("finished"),
            WaitReason::Environment(text) | WaitReason::Failed(text) | WaitReason::Other(text) => {
                f.write_str(text)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSubject {
    Task(TaskId),
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub run_id: String,
    pub subject: RunSubject,
    pub workflow: String,
    pub started: String,
    #[serde(default)]
    pub definition_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bound {
    pub after_task: TaskId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerState {
    Active,
    Stopped,
    Halted,
    Ahead,
    Prepared,
    Done,
}

impl fmt::Display for OwnerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OwnerState::Active => "active",
            OwnerState::Stopped => "stopped",
            OwnerState::Halted => "halted",
            OwnerState::Ahead => "ahead",
            OwnerState::Prepared => "prepared",
            OwnerState::Done => "done",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    #[serde(default)]
    pub lane: Option<LaneId>,
    pub state: OwnerState,
    pub since: String,
    pub by: Author,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub prepared_commit: Option<String>,
    #[serde(default)]
    pub refreshed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneState {
    Running,
    Idle,
    Paused,
    Stopped,
    Halted,
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRecord {
    pub kind: String,
    pub index: u32,
    pub state: LaneState,
    #[serde(default)]
    pub reason: Option<WaitReason>,
    #[serde(default)]
    pub job: Option<JobRecord>,
    #[serde(default)]
    pub jobs_done: u32,
    #[serde(default)]
    pub pause_requested: bool,
    #[serde(default)]
    pub stop_requested: Option<StopCause>,
    #[serde(default)]
    pub bound: Option<Bound>,
}

impl LaneRecord {
    pub fn new(id: &LaneId, state: LaneState) -> Self {
        Self {
            kind: id.kind.clone(),
            index: id.index,
            state,
            reason: None,
            job: None,
            jobs_done: 0,
            pause_requested: false,
            stop_requested: None,
            bound: None,
        }
    }

    pub fn id(&self) -> LaneId {
        LaneId::new(self.kind.clone(), self.index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LanesState {
    #[serde(default)]
    pub owners: BTreeMap<TaskId, OwnerRecord>,
    #[serde(default)]
    pub lanes: BTreeMap<LaneId, LaneRecord>,
}

impl LanesState {
    /// The task a lane currently holds: an active, stopped or halted owner naming it.
    pub fn task_of(&self, lane: &LaneId) -> Option<&TaskId> {
        self.owners.iter().find_map(|(task, owner)| {
            let holds = matches!(
                owner.state,
                OwnerState::Active | OwnerState::Stopped | OwnerState::Halted
            );
            (holds && owner.lane.as_ref() == Some(lane)).then_some(task)
        })
    }
}

/// First eight characters of a run id, for history lines.
pub fn short_run(run_id: &str) -> String {
    run_id.chars().take(8).collect()
}

// ---------------------------------------------------------------------------------------------
// Job end
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    Succeeded,
    /// The run failed; carries the failure text.
    Failed(String),
    /// The workflow halted; carries the halt text.
    Halted(String),
    Cancelled,
    /// The run is waiting on the person, so it has not ended.
    WaitingOnYou,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEndInput<'a> {
    pub outcome: JobEnd,
    /// The lane's kind runs ahead of its task's turn.
    pub lane_kind_is_ahead: bool,
    pub all_leaves_finished: bool,
    pub checks_owed: bool,
    /// The commit an ahead run prepared.
    pub prepared_commit: Option<String>,
    pub now: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub owner: OwnerRecord,
    pub lane: LaneRecord,
    pub history: Option<String>,
}

pub fn owner_after_job_end(
    owner: &OwnerRecord,
    lane: &LaneRecord,
    input: &JobEndInput<'_>,
) -> Transition {
    let mut owner = owner.clone();
    let mut lane = lane.clone();
    let mut history = None;
    match &input.outcome {
        JobEnd::Succeeded => {
            lane.state = LaneState::Idle;
            lane.job = None;
            lane.reason = None;
            lane.jobs_done += 1;
            if input.lane_kind_is_ahead {
                owner.state = OwnerState::Prepared;
                owner.lane = None;
                owner.prepared_commit = input.prepared_commit.clone();
                owner.since = input.now.to_string();
            } else if input.all_leaves_finished && !input.checks_owed {
                owner.state = OwnerState::Done;
                owner.since = input.now.to_string();
            }
        }
        JobEnd::Failed(text) | JobEnd::Halted(text) => {
            lane.state = LaneState::Halted;
            lane.reason = Some(WaitReason::Failed(text.clone()));
            owner.state = OwnerState::Halted;
            owner.reason = Some(text.clone());
            owner.by = Author::System;
            owner.since = input.now.to_string();
        }
        JobEnd::Cancelled => {
            lane.state = LaneState::Paused;
            lane.job = None;
            lane.reason = Some(WaitReason::Other("job cancelled".to_string()));
        }
        JobEnd::WaitingOnYou => {
            lane.state = LaneState::Running;
        }
    }
    if matches!(input.outcome, JobEnd::Failed(_) | JobEnd::Halted(_))
        && let Some(job) = &lane.job
    {
        history = Some(format!("run {} halted", short_run(&job.run_id)));
    }
    Transition {
        owner,
        lane,
        history,
    }
}

/// A halted lane whose failed job is live again by retry resumes running.
pub fn lane_after_retry(owner: &OwnerRecord, lane: &LaneRecord, now: &str) -> Option<Transition> {
    if lane.state != LaneState::Halted || owner.state != OwnerState::Halted {
        return None;
    }
    let run_id = lane
        .job
        .as_ref()
        .map(|job| job.run_id.as_str())
        .unwrap_or("");
    let mut owner = owner.clone();
    let mut lane = lane.clone();
    lane.state = LaneState::Running;
    lane.reason = None;
    owner.state = OwnerState::Active;
    owner.reason = None;
    owner.since = now.to_string();
    Some(Transition {
        history: Some(format!("resumed by retry of run {}", short_run(run_id))),
        owner,
        lane,
    })
}

// ---------------------------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------------------------

pub fn stop_lane(lane: &LaneRecord, cause: StopCause) -> LaneRecord {
    let mut lane = lane.clone();
    match lane.state {
        LaneState::Running => lane.stop_requested = Some(cause),
        LaneState::Idle | LaneState::Paused | LaneState::Waiting => {
            lane.state = LaneState::Stopped;
            lane.reason = None;
        }
        LaneState::Stopped | LaneState::Halted => {}
    }
    lane
}

/// An active owner whose lane stopped becomes stopped; every other owner state is unchanged
/// (a stop never changes a halted owner).
pub fn owner_after_stop(owner: &OwnerRecord, cause: StopCause, now: &str) -> OwnerRecord {
    let mut owner = owner.clone();
    if owner.state == OwnerState::Active {
        owner.state = OwnerState::Stopped;
        owner.since = now.to_string();
        owner.by = Author::System;
        owner.reason = Some(
            match cause {
                StopCause::Stop => "lanes stopped",
                StopCause::Count => "lane count lowered",
                StopCause::Rollout => "rollout",
                StopCause::Bound => "bound reached",
            }
            .to_string(),
        );
    }
    owner
}

/// Applies a pending stop when a lane's job has just ended and the lane is idle: the lane stops
/// and its owner stops with it, keeping the lane.
pub fn stop_at_job_boundary(
    lane: &LaneRecord,
    owner: Option<&OwnerRecord>,
    now: &str,
) -> (LaneRecord, Option<OwnerRecord>) {
    let Some(cause) = lane.stop_requested else {
        return (lane.clone(), owner.cloned());
    };
    if lane.state != LaneState::Idle {
        return (lane.clone(), owner.cloned());
    }
    let mut lane = lane.clone();
    lane.state = LaneState::Stopped;
    lane.stop_requested = None;
    let owner = owner.map(|owner| owner_after_stop(owner, cause, now));
    (lane, owner)
}

// ---------------------------------------------------------------------------------------------
// Count reconciliation
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CountPlan {
    /// Lanes up to the count that do not exist yet; they start idle.
    pub create: Vec<LaneId>,
    /// Lanes above the count (every one listed in `remove_now` or `stop_after_job`).
    pub lower: Vec<LaneId>,
    /// Idle, paused or stopped lanes above the count: stopped and removed at once.
    pub remove_now: Vec<LaneId>,
    /// Running lanes above the count: stop after their job, with cause `Count`.
    pub stop_after_job: Vec<LaneId>,
    /// Owners stopped (or stopping) on a lane that is gone, free for any lane to adopt.
    pub adoptable: Vec<TaskId>,
}

pub fn reconcile_counts(counts: &BTreeMap<String, u32>, state: &LanesState) -> CountPlan {
    let mut plan = CountPlan::default();
    for (kind, count) in counts {
        for index in 1..=*count {
            let id = LaneId::new(kind.clone(), index);
            if !state.lanes.contains_key(&id) {
                plan.create.push(id);
            }
        }
    }
    let beyond = |id: &LaneId| id.index > counts.get(&id.kind).copied().unwrap_or(0);
    for (id, lane) in &state.lanes {
        if !beyond(id) {
            continue;
        }
        match lane.state {
            LaneState::Halted => {}
            LaneState::Running => {
                plan.lower.push(id.clone());
                plan.stop_after_job.push(id.clone());
            }
            LaneState::Idle | LaneState::Paused | LaneState::Stopped | LaneState::Waiting => {
                plan.lower.push(id.clone());
                plan.remove_now.push(id.clone());
            }
        }
    }
    for (task, owner) in &state.owners {
        let Some(lane) = &owner.lane else { continue };
        let gone = !state.lanes.contains_key(lane) || plan.remove_now.contains(lane);
        let stopping = owner.state == OwnerState::Stopped
            || (owner.state == OwnerState::Active && plan.remove_now.contains(lane));
        if stopping && gone && beyond(lane) {
            plan.adoptable.push(task.clone());
        }
    }
    plan
}

/// Whether `candidate` may take over `owner` on its own (without a manual adopt). A stopped
/// owner whose lane still exists is resumed only by that lane.
pub fn owner_adoptable_by(owner: &OwnerRecord, lane_exists: bool, candidate: &LaneId) -> bool {
    match owner.state {
        OwnerState::Stopped => match &owner.lane {
            Some(lane) => !lane_exists || lane == candidate,
            None => true,
        },
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Adopt
// ---------------------------------------------------------------------------------------------

/// Where a pin sends a task: a specific lane, a lane kind, or both constraints.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PinTarget {
    pub lane: Option<String>,
    pub kind: Option<String>,
}

impl PinTarget {
    pub fn allows(&self, lane: &LaneId) -> bool {
        self.lane.as_deref().is_none_or(|l| l == lane.to_string())
            && self.kind.as_deref().is_none_or(|k| k == lane.kind)
    }
}

impl From<&Pin> for PinTarget {
    fn from(pin: &Pin) -> Self {
        Self {
            lane: pin.lane.as_ref().map(|l| l.value.clone()),
            kind: pin.kind.as_ref().map(|k| k.value.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptRequest {
    pub task: TaskId,
    /// The lane that takes the task.
    pub lane: LaneId,
    pub reason: String,
    pub by: Author,
    pub since: String,
    /// Whether the halted run's working tree can still be read.
    pub old_tree_readable: bool,
    /// The person agreed to drop unreadable work.
    pub confirm_drop: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdoptRefusal {
    #[error("an adopt needs a reason")]
    BlankReason,
    #[error("{0} is not a known lane")]
    UnknownLane(LaneId),
    #[error("{0} has no owner record")]
    UnknownTask(TaskId),
    #[error("{task} is owned by {lane}, which is running")]
    OwnerRunning { task: TaskId, lane: LaneId },
    #[error("{task} is {state}, and only a stopped or halted task can be adopted")]
    NotAdoptable { task: TaskId, state: OwnerState },
    #[error("{task} is pinned elsewhere, so {lane} cannot take it")]
    PinnedElsewhere { task: TaskId, lane: LaneId },
    #[error("{lane} is not free: {why}")]
    LaneBusy { lane: LaneId, why: String },
    #[error("the old working tree of {0} cannot be read; confirm to drop that work")]
    UnreadableTree(TaskId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptOutcome {
    pub owner: OwnerRecord,
    /// The lane the task came from, now idle, when it still exists.
    pub source_lane: Option<(LaneId, LaneRecord)>,
    pub history: String,
    /// A halted run the caller must cancel.
    pub cancel_run: Option<String>,
    pub cancel_history: Option<String>,
    pub dropped_work: bool,
}

pub fn adopt(
    state: &LanesState,
    req: &AdoptRequest,
    pinned_to: Option<&PinTarget>,
) -> Result<AdoptOutcome, AdoptRefusal> {
    if req.reason.trim().is_empty() {
        return Err(AdoptRefusal::BlankReason);
    }
    let Some(target) = state.lanes.get(&req.lane) else {
        return Err(AdoptRefusal::UnknownLane(req.lane.clone()));
    };
    let Some(owner) = state.owners.get(&req.task) else {
        return Err(AdoptRefusal::UnknownTask(req.task.clone()));
    };
    let source_lane = owner
        .lane
        .as_ref()
        .and_then(|id| state.lanes.get(id).map(|lane| (id.clone(), lane)));
    if let Some((id, lane)) = &source_lane
        && owner.state == OwnerState::Active
        && lane.state == LaneState::Running
    {
        return Err(AdoptRefusal::OwnerRunning {
            task: req.task.clone(),
            lane: id.clone(),
        });
    }
    if !matches!(owner.state, OwnerState::Stopped | OwnerState::Halted) {
        return Err(AdoptRefusal::NotAdoptable {
            task: req.task.clone(),
            state: owner.state,
        });
    }
    if let Some(pin) = pinned_to
        && !pin.allows(&req.lane)
    {
        return Err(AdoptRefusal::PinnedElsewhere {
            task: req.task.clone(),
            lane: req.lane.clone(),
        });
    }
    if let Some(held) = state.task_of(&req.lane) {
        return Err(AdoptRefusal::LaneBusy {
            lane: req.lane.clone(),
            why: format!("it owns {held}"),
        });
    }
    if target.state != LaneState::Idle {
        return Err(AdoptRefusal::LaneBusy {
            lane: req.lane.clone(),
            why: "it is not idle".to_string(),
        });
    }
    let halted = owner.state == OwnerState::Halted;
    let dropped_work = halted && !req.old_tree_readable;
    if dropped_work && !req.confirm_drop {
        return Err(AdoptRefusal::UnreadableTree(req.task.clone()));
    }
    let cancel_run = if halted {
        source_lane
            .as_ref()
            .and_then(|(_, lane)| lane.job.as_ref())
            .map(|job| job.run_id.clone())
    } else {
        None
    };
    let cancel_history = cancel_run
        .as_ref()
        .map(|run| format!("run {} cancelled by adopt to {}", short_run(run), req.lane));
    let from = owner
        .lane
        .as_ref()
        .map_or_else(|| "nobody".to_string(), |lane| lane.to_string());
    let mut history = format!(
        "adopted by {} from {}: {} ({})",
        req.lane,
        from,
        req.reason.trim(),
        req.by
    );
    if dropped_work {
        history.push_str("; unreadable old work dropped");
    }
    let new_source = source_lane.and_then(|(id, lane)| {
        (id != req.lane).then(|| {
            let mut lane = lane.clone();
            lane.state = LaneState::Idle;
            lane.job = None;
            lane.reason = None;
            lane.stop_requested = None;
            (id, lane)
        })
    });
    Ok(AdoptOutcome {
        owner: OwnerRecord {
            lane: Some(req.lane.clone()),
            state: OwnerState::Active,
            since: req.since.clone(),
            by: req.by.clone(),
            reason: Some(req.reason.trim().to_string()),
            prepared_commit: owner.prepared_commit.clone(),
            refreshed: owner.refreshed,
        },
        source_lane: new_source,
        history,
        cancel_run,
        cancel_history,
        dropped_work,
    })
}
