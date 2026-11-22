//! The graph stamp: a digest of the structure of the task graph and of the lane table's pins and
//! exemptions, recorded in `stamp.toml` so the picker refuses to pick once the structure changed
//! since a person accepted it.
//!
//! [`compute`] takes the [`LaneTable`] as well as the [`PickGraph`] because pins and exemptions
//! live in the table. It is pure and deterministic: the result depends only on the content of its
//! inputs, never on the order tasks, pins or exemptions were listed in. Statuses, todos, goals and
//! run state are not part of the stamp, so work progressing never changes it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::graph::{EdgeKind, PickGraph, PickTask, Shape};
use crate::state::TaskId;
use crate::table::LaneTable;

/// The `schema` of `stamp.toml` this version reads and writes.
pub const STAMP_SCHEMA: u32 = 1;

/// The stamp of one graph and lane table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphStamp {
    /// sha256 over the canonical text of every task and of the pins and exemptions.
    pub digest: [u8; 32],
    /// The first eight bytes of the sha256 of each task's canonical text.
    pub per_task: BTreeMap<TaskId, [u8; 8]>,
    /// Each task's goal, only to name it in a [`StampDiff`]; not part of the digest.
    pub names: BTreeMap<TaskId, String>,
}

impl GraphStamp {
    pub fn digest_hex(&self) -> String {
        hex(&self.digest)
    }

    /// Whether the recorded stamp has the same digest.
    pub fn matches(&self, recorded: &StampFile) -> bool {
        recorded.digest.eq_ignore_ascii_case(&self.digest_hex())
    }
}

/// Computes the stamp. Per task reachable from the top, in id order: its id, parent id, shape,
/// work kind, sorted own areas, size (unset counts as 1) and sorted edges (kind, target id,
/// unblock point); then the table's pins and exemptions, each in sorted order. Every string is
/// written JSON-quoted so no value can run into the next field.
pub fn compute(graph: &PickGraph, table: &LaneTable) -> GraphStamp {
    let mut tasks: Vec<&PickTask> = graph.walk().collect();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));

    let mut hasher = Sha256::new();
    let mut per_task = BTreeMap::new();
    let mut names = BTreeMap::new();
    for task in tasks {
        let text = task_text(graph, task);
        hasher.update(text.as_bytes());
        let hash = Sha256::digest(text.as_bytes());
        let mut short = [0u8; 8];
        short.copy_from_slice(&hash[..8]);
        per_task.insert(task.id.clone(), short);
        names.insert(task.id.clone(), task.name.clone());
    }
    hasher.update(table_text(table).as_bytes());
    GraphStamp {
        digest: hasher.finalize().into(),
        per_task,
        names,
    }
}

fn quote(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

fn quote_opt(text: Option<&str>) -> String {
    text.map_or_else(|| "-".to_string(), quote)
}

fn task_text(graph: &PickGraph, task: &PickTask) -> String {
    let mut out = String::new();
    let shape = match task.shape {
        Shape::Group => "group",
        Shape::Leaf => "leaf",
    };
    let areas: BTreeSet<String> = task.areas.iter().map(|area| quote(area)).collect();
    let edges: BTreeSet<String> = task
        .edges
        .iter()
        .map(|edge| {
            let kind = match edge.kind {
                EdgeKind::After => "after",
                EdgeKind::StackedOn => "stacked_on",
            };
            format!(
                "edge {kind} {} {}",
                quote(&edge.target.0),
                quote(&edge.unblock)
            )
        })
        .collect();
    // Writing to a String cannot fail.
    let _ = writeln!(out, "task {}", quote(&task.id.0));
    let _ = writeln!(
        out,
        "parent {}",
        quote_opt(graph.parent(&task.id).map(|parent| parent.0.as_str()))
    );
    let _ = writeln!(out, "shape {shape}");
    let _ = writeln!(out, "work_kind {}", quote_opt(task.work_kind.as_deref()));
    let _ = writeln!(out, "areas {}", join(&areas));
    let _ = writeln!(out, "size {}", task.size.unwrap_or(1));
    for edge in edges {
        let _ = writeln!(out, "{edge}");
    }
    out
}

fn join(items: &BTreeSet<String>) -> String {
    items.iter().cloned().collect::<Vec<_>>().join(" ")
}

fn table_text(table: &LaneTable) -> String {
    let mut pins: Vec<String> = table
        .pins
        .iter()
        .map(|pin| {
            format!(
                "pin {} {} {}\n",
                quote_opt(pin.task.as_ref().map(|task| task.value.as_str())),
                quote_opt(pin.lane.as_ref().map(|lane| lane.value.as_str())),
                quote_opt(pin.kind.as_ref().map(|kind| kind.value.as_str())),
            )
        })
        .collect();
    pins.sort();
    let mut exemptions: Vec<String> = table
        .exemptions
        .iter()
        .map(|exempt| {
            let tags: BTreeSet<String> = exempt.tags.iter().map(|tag| quote(&tag.value)).collect();
            let tasks: BTreeSet<String> = exempt
                .tasks
                .iter()
                .map(|task| quote(&task.value))
                .collect();
            format!("exempt tags {} tasks {}\n", join(&tags), join(&tasks))
        })
        .collect();
    exemptions.sort();
    let mut out = String::from("pins\n");
    out.extend(pins);
    out.push_str("exemptions\n");
    out.extend(exemptions);
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------------------------
// stamp.toml
// ---------------------------------------------------------------------------------------------

/// `stamp.toml`: the schema, the 64-hex digest and a `[tasks]` table of 16-hex per-task hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StampFile {
    pub schema: u32,
    pub digest: String,
    #[serde(default)]
    pub tasks: BTreeMap<TaskId, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StampError {
    #[error("stamp.toml does not parse: {0}")]
    Toml(String),
    #[error("stamp.toml has schema {0}; this version reads schema {STAMP_SCHEMA}")]
    Schema(u32),
    #[error("stamp.toml digest {0:?} is not 64 hex digits")]
    Digest(String),
    #[error("stamp.toml hash {value:?} of task {task} is not 16 hex digits")]
    TaskHash { task: TaskId, value: String },
}

impl StampFile {
    pub fn from_stamp(stamp: &GraphStamp) -> Self {
        Self {
            schema: STAMP_SCHEMA,
            digest: stamp.digest_hex(),
            tasks: stamp
                .per_task
                .iter()
                .map(|(task, hash)| (task.clone(), hex(hash)))
                .collect(),
        }
    }

    /// Parses and checks `stamp.toml`. Malformed text is an error, never a panic.
    pub fn parse(text: &str) -> Result<Self, StampError> {
        let file: StampFile =
            toml::from_str(text).map_err(|error| StampError::Toml(error.message().to_string()))?;
        if file.schema != STAMP_SCHEMA {
            return Err(StampError::Schema(file.schema));
        }
        if !is_hex(&file.digest, 64) {
            return Err(StampError::Digest(file.digest));
        }
        if let Some((task, value)) = file.tasks.iter().find(|(_, value)| !is_hex(value, 16)) {
            return Err(StampError::TaskHash {
                task: task.clone(),
                value: value.clone(),
            });
        }
        Ok(file)
    }

    /// Renders `stamp.toml` with a stable key order: schema, digest, then tasks by id.
    pub fn render(&self) -> Result<String, StampError> {
        toml::to_string(self).map_err(|error| StampError::Toml(error.to_string()))
    }
}

// ---------------------------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------------------------

/// A task named in a [`StampDiff`]: its goal when the current graph has it, else its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampTask {
    pub id: TaskId,
    pub name: String,
}

/// What changed between a recorded stamp and the current one, each list in id order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampDiff {
    pub added: Vec<StampTask>,
    pub changed: Vec<StampTask>,
    pub removed: Vec<StampTask>,
    /// The digests differ while every task hash matches: the pins or exemptions changed.
    #[serde(default)]
    pub table_changed: bool,
}

impl StampDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.changed.is_empty()
            && self.removed.is_empty()
            && !self.table_changed
    }

    /// The first task the diff names: changed, then added, then removed.
    pub fn first_task(&self) -> Option<&TaskId> {
        self.changed
            .iter()
            .chain(&self.added)
            .chain(&self.removed)
            .map(|task| &task.id)
            .next()
    }
}

/// Compares the recorded stamp with the current one. Equal digests give an empty diff. Per-task
/// hashes only say that a task changed, not which part of it.
pub fn diff(recorded: &StampFile, current: &GraphStamp) -> StampDiff {
    if current.matches(recorded) {
        return StampDiff::default();
    }
    let named = |id: &TaskId| StampTask {
        id: id.clone(),
        name: current
            .names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.0.clone()),
    };
    let mut out = StampDiff::default();
    for (id, hash) in &current.per_task {
        match recorded.tasks.get(id) {
            None => out.added.push(named(id)),
            Some(value) if !value.eq_ignore_ascii_case(&hex(hash)) => out.changed.push(named(id)),
            Some(_) => {}
        }
    }
    out.removed = recorded
        .tasks
        .keys()
        .filter(|id| !current.per_task.contains_key(*id))
        .map(named)
        .collect();
    out.table_changed = out.added.is_empty() && out.changed.is_empty() && out.removed.is_empty();
    out
}

impl fmt::Display for StampDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("graph matches the stamp");
        }
        f.write_str("graph changed since the stamp: ")?;
        let mut parts = Vec::new();
        for (verb, tasks) in [
            ("added", &self.added),
            ("changed", &self.changed),
            ("removed", &self.removed),
        ] {
            if !tasks.is_empty() {
                let names: Vec<&str> = tasks.iter().map(|task| task.name.as_str()).collect();
                parts.push(format!("{verb} {}", names.join(", ")));
            }
        }
        if self.table_changed {
            parts.push("pins or exemptions changed".to_string());
        }
        f.write_str(&parts.join("; "))
    }
}
