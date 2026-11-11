//! The run record: `run.toml` (the run's identity) and the checkpoint log `events.jsonl` of each
//! attempt, one JSON object per line.
//!
//! The run state is never stored: [`replay`] rebuilds it from the events, so it cannot disagree with
//! the log. [`append`] writes each event with one `write_all` followed by `sync_data`, so a node
//! that finished is on disk before anything routes on it. A crash mid-write leaves at most a
//! truncated last line, which [`parse_log`] drops and reports.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::failure::Failure;
use crate::inputs::InputValue;

/// The `schema` of `run.toml` this crate writes.
pub const RUN_SCHEMA: u32 = 1;

/// One node instance: the node, the fan-out branch it runs in (outermost fan first, as `(fan node,
/// item index)` pairs) and its pass (1 for the first, counted up by loops).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeKey {
    pub node: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branch: Vec<(String, u32)>,
    pub pass: u32,
}

impl NodeKey {
    pub fn new(node: impl Into<String>, pass: u32) -> Self {
        Self {
            node: node.into(),
            branch: Vec::new(),
            pass,
        }
    }

    pub fn in_branch(mut self, fan: impl Into<String>, index: u32) -> Self {
        self.branch.push((fan.into(), index));
        self
    }

    /// The branch as a directory name: `root` outside any fan-out, else `plan-0.items-2`.
    pub fn branch_label(&self) -> String {
        branch_label(&self.branch)
    }
}

/// [`NodeKey::branch_label`] for a bare branch.
pub fn branch_label(branch: &[(String, u32)]) -> String {
    if branch.is_empty() {
        return "root".to_string();
    }
    branch
        .iter()
        .map(|(fan, index)| format!("{fan}-{index}"))
        .collect::<Vec<_>>()
        .join(".")
}

/// The stable string form: `node#pass` with one `[fan=index]` per branch level before the `#`, as
/// in `review[plan=0][items=2]#1`.
impl fmt::Display for NodeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.node)?;
        for (fan, index) in &self.branch {
            write!(formatter, "[{fan}={index}]")?;
        }
        write!(formatter, "#{}", self.pass)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a node key: '{0}'")]
pub struct NodeKeyError(pub String);

impl FromStr for NodeKey {
    type Err = NodeKeyError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let error = || NodeKeyError(text.to_string());
        let (head, pass) = text.rsplit_once('#').ok_or_else(error)?;
        let pass = pass.parse().map_err(|_| error())?;
        let (node, mut rest) = match head.find('[') {
            Some(at) => (&head[..at], &head[at..]),
            None => (head, ""),
        };
        if node.is_empty() {
            return Err(error());
        }
        let mut branch = Vec::new();
        while !rest.is_empty() {
            let inner = rest.strip_prefix('[').ok_or_else(error)?;
            let close = inner.find(']').ok_or_else(error)?;
            let (fan, index) = inner[..close].split_once('=').ok_or_else(error)?;
            if fan.is_empty() {
                return Err(error());
            }
            branch.push((fan.to_string(), index.parse().map_err(|_| error())?));
            rest = &inner[close + 1..];
        }
        Ok(Self {
            node: node.to_string(),
            branch,
            pass,
        })
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndState {
    Succeeded,
    Failed,
}

/// One checkpoint in `events.jsonl`, tagged by `event`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    Created,
    Queued,
    NodeStarted {
        key: NodeKey,
        #[serde(rename = "try")]
        try_number: u32,
        pid: u32,
        pid_start_time: u64,
    },
    /// Exactly one of `result` and `failure` is set.
    NodeFinished {
        key: NodeKey,
        #[serde(rename = "try")]
        try_number: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<Failure>,
        #[serde(default)]
        cost_usd: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns: Option<u32>,
        #[serde(default)]
        elapsed_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    GateOpened {
        key: NodeKey,
        question: String,
    },
    GateClosed {
        key: NodeKey,
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
    },
    BranchMerged {
        join: String,
        branch: Vec<(String, u32)>,
        sha: String,
    },
    RouteTaken {
        from: NodeKey,
        to: String,
        pass: u32,
    },
    Paused,
    Halted,
    Resumed,
    Interrupted {
        keys: Vec<NodeKey>,
    },
    DefinitionReloaded {
        rev: u32,
    },
    Ended {
        state: EndState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<Failure>,
    },
    Cancelled,
}

impl Event {
    /// The event as one line of `events.jsonl`, with its newline.
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("events always serialize");
        line.push('\n');
        line
    }
}

/// A file the event log can be appended to: [`Write`] plus a data sync.
pub trait SyncWrite: Write {
    fn sync_data(&mut self) -> io::Result<()>;
}

impl SyncWrite for File {
    fn sync_data(&mut self) -> io::Result<()> {
        File::sync_data(self)
    }
}

/// Appends `event` to the open log: one line, written with a single `write_all`, then `sync_data`.
pub fn append<W: SyncWrite + ?Sized>(file: &mut W, event: &Event) -> io::Result<()> {
    file.write_all(event.to_line().as_bytes())?;
    file.sync_data()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("events.jsonl line {line}: {message}")]
pub struct LogError {
    pub line: usize,
    pub message: String,
}

/// Parses `events.jsonl`. Blank lines are skipped. A last line that does not parse (a crash
/// mid-write) is dropped, and the second value says so; a bad line before it is an error, because
/// the log was written one whole line at a time and something else damaged it.
pub fn parse_log(text: &str) -> Result<(Vec<Event>, bool), LogError> {
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| (index + 1, line))
        .collect();
    let mut events = Vec::with_capacity(lines.len());
    for (position, (number, line)) in lines.iter().enumerate() {
        match serde_json::from_str::<Event>(line) {
            Ok(event) => events.push(event),
            Err(_) if position + 1 == lines.len() => return Ok((events, true)),
            Err(error) => {
                return Err(LogError {
                    line: *number,
                    message: error.to_string(),
                });
            }
        }
    }
    Ok((events, false))
}

/// Who started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartedBy {
    Kyle,
    TreeRun,
    Healer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoRef {
    pub index: usize,
    pub name: String,
}

/// A resolved input as `run.toml` keeps it. A value that was missing is not written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedInput {
    pub name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<InputValue>,
}

/// One attempt of a run. A restart adds one; earlier attempts stay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub n: u32,
    pub created: String,
    pub definition_hash: String,
    pub worktree: String,
    pub branch: String,
}

/// `runs/<run id>/run.toml`: the run's identity, written at create and when an attempt is added.
/// Times are RFC 3339 strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunToml {
    pub schema: u32,
    /// A UUID v7 ([`is_uuid_v7`]).
    pub id: String,
    pub workflow: String,
    /// `shipped`, `profile` or `project`.
    pub layer: String,
    pub task_id: String,
    pub created: String,
    pub started_by: StartedBy,
    pub todo: TodoRef,
    #[serde(default)]
    pub inputs: Vec<RecordedInput>,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunTomlError {
    #[error("run.toml does not parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("run.toml has schema {0}, this version reads {RUN_SCHEMA}")]
    Schema(u32),
    #[error("run.toml id '{0}' is not a UUID v7")]
    Id(String),
}

impl RunToml {
    pub fn parse(text: &str) -> Result<Self, RunTomlError> {
        let run: Self = toml::from_str(text)?;
        if run.schema != RUN_SCHEMA {
            return Err(RunTomlError::Schema(run.schema));
        }
        if !is_uuid_v7(&run.id) {
            return Err(RunTomlError::Id(run.id));
        }
        Ok(run)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("run.toml always serializes")
    }

    /// The newest attempt.
    pub fn current_attempt(&self) -> Option<&Attempt> {
        self.attempts.iter().max_by_key(|attempt| attempt.n)
    }
}

/// Whether `id` is a hyphenated UUID of version 7 with the RFC 9562 variant.
pub fn is_uuid_v7(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let hyphen = matches!(index, 8 | 13 | 18 | 23);
        if hyphen != (*byte == b'-') || (!hyphen && !byte.is_ascii_hexdigit()) {
            return false;
        }
    }
    bytes[14] == b'7' && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

/// A node try that started and has not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningNode {
    pub try_number: u32,
    pub pid: u32,
    pub pid_start_time: u64,
}

/// A finished node try, as `NodeFinished` recorded it.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedNode {
    pub key: NodeKey,
    pub try_number: u32,
    pub result: Option<serde_json::Value>,
    pub failure: Option<Failure>,
    pub cost_usd: f64,
    pub turns: Option<u32>,
    pub elapsed_ms: u64,
    pub session_id: Option<String>,
    /// Whether the log had a `NodeStarted` for this try. A finish without one is still recorded:
    /// replay never invents or discards what the log says.
    pub had_start: bool,
}

impl FinishedNode {
    pub fn succeeded(&self) -> bool {
        self.failure.is_none()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClosedGate {
    pub key: NodeKey,
    pub by: String,
    pub edit: Option<serde_json::Value>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedBranch {
    pub join: String,
    pub branch: Vec<(String, u32)>,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenRoute {
    pub from: NodeKey,
    pub to: String,
    pub pass: u32,
}

/// The run's lifecycle as one value, derived from [`RunState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// No `Created` event yet (an empty log).
    New,
    Created,
    Queued,
    Running,
    Paused,
    Halted,
    Ended(EndState),
    Cancelled,
}

/// The state of one attempt, rebuilt from its events by [`replay`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunState {
    pub created: bool,
    pub queued: bool,
    /// Started and not finished or interrupted.
    pub running: BTreeMap<NodeKey, RunningNode>,
    /// Every finished try in log order.
    pub finished: Vec<FinishedNode>,
    /// The sum of `cost_usd` over every finished try.
    pub cost_usd: f64,
    /// Gates open now, with their question.
    pub open_gates: BTreeMap<NodeKey, String>,
    pub closed_gates: Vec<ClosedGate>,
    pub merged: Vec<MergedBranch>,
    pub routes: Vec<TakenRoute>,
    /// The highest pass each node reached, from starts and routes.
    pub passes: BTreeMap<String, u32>,
    pub paused: bool,
    pub halted: bool,
    /// Keys the log marked interrupted and that have not started again. The value is the try that
    /// was running, `None` when the key was not running when interrupted.
    pub interrupted: BTreeMap<NodeKey, Option<RunningNode>>,
    /// The definition revision in use: `None` until a `DefinitionReloaded`, meaning revision 0.
    pub definition_rev: Option<u32>,
    pub ended: Option<(EndState, Option<Failure>)>,
    pub cancelled: bool,
    /// Events after `Ended` or `Cancelled`. They are not applied: an attempt is over once it
    /// ends (a restart is a new attempt with its own log), so these can only be damage; the count
    /// lets the caller flag them.
    pub ignored_after_end: usize,
}

impl RunState {
    pub fn lifecycle(&self) -> Lifecycle {
        if self.cancelled {
            Lifecycle::Cancelled
        } else if let Some((state, _)) = &self.ended {
            Lifecycle::Ended(*state)
        } else if self.halted {
            Lifecycle::Halted
        } else if self.paused {
            Lifecycle::Paused
        } else if !self.running.is_empty() || !self.finished.is_empty() {
            Lifecycle::Running
        } else if self.queued {
            Lifecycle::Queued
        } else if self.created {
            Lifecycle::Created
        } else {
            Lifecycle::New
        }
    }

    pub fn is_over(&self) -> bool {
        self.cancelled || self.ended.is_some()
    }

    /// The last finished try of `key`.
    pub fn last_finish(&self, key: &NodeKey) -> Option<&FinishedNode> {
        self.finished
            .iter()
            .rev()
            .find(|finished| &finished.key == key)
    }
}

/// Rebuilds the run state from its events. Pure: the same events always give the same state.
pub fn replay(events: &[Event]) -> RunState {
    let mut state = RunState::default();
    for event in events {
        if state.is_over() {
            state.ignored_after_end += 1;
            continue;
        }
        apply(&mut state, event);
    }
    state
}

fn bump_pass(state: &mut RunState, node: &str, pass: u32) {
    let entry = state.passes.entry(node.to_string()).or_insert(pass);
    *entry = (*entry).max(pass);
}

fn apply(state: &mut RunState, event: &Event) {
    match event {
        Event::Created => state.created = true,
        Event::Queued => state.queued = true,
        Event::NodeStarted {
            key,
            try_number,
            pid,
            pid_start_time,
        } => {
            state.interrupted.remove(key);
            bump_pass(state, &key.node, key.pass);
            state.running.insert(
                key.clone(),
                RunningNode {
                    try_number: *try_number,
                    pid: *pid,
                    pid_start_time: *pid_start_time,
                },
            );
        }
        Event::NodeFinished {
            key,
            try_number,
            result,
            failure,
            cost_usd,
            turns,
            elapsed_ms,
            session_id,
        } => {
            let had_start = match state.running.get(key) {
                Some(running) if running.try_number == *try_number => {
                    state.running.remove(key);
                    true
                }
                _ => false,
            };
            state.cost_usd += cost_usd;
            state.finished.push(FinishedNode {
                key: key.clone(),
                try_number: *try_number,
                result: result.clone(),
                failure: failure.clone(),
                cost_usd: *cost_usd,
                turns: *turns,
                elapsed_ms: *elapsed_ms,
                session_id: session_id.clone(),
                had_start,
            });
        }
        Event::GateOpened { key, question } => {
            state.open_gates.insert(key.clone(), question.clone());
        }
        Event::GateClosed {
            key,
            by,
            edit,
            comment,
        } => {
            state.open_gates.remove(key);
            state.closed_gates.push(ClosedGate {
                key: key.clone(),
                by: by.clone(),
                edit: edit.clone(),
                comment: comment.clone(),
            });
        }
        Event::BranchMerged { join, branch, sha } => state.merged.push(MergedBranch {
            join: join.clone(),
            branch: branch.clone(),
            sha: sha.clone(),
        }),
        Event::RouteTaken { from, to, pass } => {
            bump_pass(state, to, *pass);
            state.routes.push(TakenRoute {
                from: from.clone(),
                to: to.clone(),
                pass: *pass,
            });
        }
        Event::Paused => state.paused = true,
        Event::Halted => state.halted = true,
        Event::Resumed => {
            state.paused = false;
            state.halted = false;
        }
        Event::Interrupted { keys } => {
            for key in keys {
                let running = state.running.remove(key);
                state.interrupted.insert(key.clone(), running);
            }
        }
        Event::DefinitionReloaded { rev } => state.definition_rev = Some(*rev),
        Event::Ended {
            state: end,
            failure,
        } => {
            state.ended = Some((*end, failure.clone()));
        }
        Event::Cancelled => state.cancelled = true,
    }
}
