//! The operations of a `graph_edits` output field (D2): `{ "project"?, "ops": [op] }`, each op one
//! of [`GraphOp::NAMES`]. The JSON schema the agent sees is in `bongos_workflow_model::schema`;
//! these types match it field for field. [`validate_batch`] is the hand check run before anything
//! is applied: an unknown op, a duplicate `ref`, an op naming a ref nothing created, an empty
//! goal.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bongos_work_model::{TaskId, UnblockPoint};
use bongos_work_model::rules;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A task named by an op: a task id, or `@<ref>`, the `ref` of a task a create of this run made.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum TaskRef {
    Id(TaskId),
    /// The ref name, without the `@`.
    Ref(String),
}

impl From<String> for TaskRef {
    fn from(text: String) -> Self {
        match text.strip_prefix('@') {
            Some(name) => TaskRef::Ref(name.to_string()),
            None => TaskRef::Id(text),
        }
    }
}

impl From<TaskRef> for String {
    fn from(task: TaskRef) -> String {
        task.to_string()
    }
}

impl fmt::Display for TaskRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskRef::Id(id) => f.write_str(id),
            TaskRef::Ref(name) => write!(f, "@{name}"),
        }
    }
}

/// The kind of an ordering edge, as an op writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    After,
    StackedOn,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::After => "after",
            EdgeKind::StackedOn => "stacked_on",
        }
    }

    pub fn to_rules(self) -> rules::EdgeKind {
        match self {
            EdgeKind::After => rules::EdgeKind::After,
            EdgeKind::StackedOn => rules::EdgeKind::StackedOn,
        }
    }
}

/// A todo of a created task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTodo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
}

/// One graph operation. `from` of an edge is the task that holds it: `from` goes after (or is
/// stacked on) `to`, which must be its sibling.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphOp {
    Create {
        /// The name later ops use as `@<ref>`; unique per run.
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        parent: TaskRef,
        goal: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        areas: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u32>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        repositories: Vec<String>,
        /// The task's todos; a create without any gets the default todos of a new leaf.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        todos: Vec<NewTodo>,
    },
    /// Retitle (`goal`), change work kind, resize (`size`) or retag (`areas`).
    Set {
        task: TaskRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        areas: Option<Vec<String>>,
    },
    /// To another parent in the same project.
    Move { task: TaskRef, parent: TaskRef },
    /// The task and its whole subtree.
    Remove { task: TaskRef },
    AddEdge {
        from: TaskRef,
        to: TaskRef,
        kind: EdgeKind,
        /// Defaults to `merged` for `after` and `review` for `stacked_on`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unblock: Option<UnblockPoint>,
    },
    RemoveEdge {
        from: TaskRef,
        to: TaskRef,
        kind: EdgeKind,
    },
}

impl GraphOp {
    /// Every op name, in the order of the schema's `oneOf`.
    pub const NAMES: [&'static str; 6] =
        ["create", "set", "move", "remove", "add_edge", "remove_edge"];

    /// The op's name as JSON writes it.
    pub fn name(&self) -> &'static str {
        match self {
            GraphOp::Create { .. } => "create",
            GraphOp::Set { .. } => "set",
            GraphOp::Move { .. } => "move",
            GraphOp::Remove { .. } => "remove",
            GraphOp::AddEdge { .. } => "add_edge",
            GraphOp::RemoveEdge { .. } => "remove_edge",
        }
    }

    /// The tasks the op names, in field order.
    pub fn task_refs(&self) -> Vec<&TaskRef> {
        match self {
            GraphOp::Create { parent, .. } => vec![parent],
            GraphOp::Set { task, .. } | GraphOp::Remove { task } => vec![task],
            GraphOp::Move { task, parent } => vec![task, parent],
            GraphOp::AddEdge { from, to, .. } | GraphOp::RemoveEdge { from, to, .. } => {
                vec![from, to]
            }
        }
    }
}

/// The value of a `graph_edits` field.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphEditBatch {
    /// The project the ops edit; absent means the run's own project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub ops: Vec<GraphOp>,
}

/// Why a batch is malformed. `op` is the 1-based position of the op at fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchError {
    pub op: Option<usize>,
    pub message: String,
}

impl BatchError {
    fn at(index: usize, message: impl Into<String>) -> Self {
        Self {
            op: Some(index + 1),
            message: message.into(),
        }
    }
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.op {
            Some(op) => write!(f, "op {op}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for BatchError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBatch {
    #[serde(default)]
    project: Option<String>,
    ops: Vec<Value>,
}

/// Reads a `graph_edits` value and checks it with [`check_batch`]. `known_refs` are the refs
/// earlier batches of the run created.
pub fn validate_batch(
    value: &Value,
    known_refs: &BTreeMap<String, TaskId>,
) -> Result<GraphEditBatch, BatchError> {
    let raw: RawBatch = serde_json::from_value(value.clone()).map_err(|error| BatchError {
        op: None,
        message: format!("graph edits: {error}"),
    })?;
    let mut ops = Vec::with_capacity(raw.ops.len());
    for (index, op) in raw.ops.into_iter().enumerate() {
        let name = op.get("op").and_then(Value::as_str);
        match name {
            None => return Err(BatchError::at(index, "names no op")),
            Some(name) if !GraphOp::NAMES.contains(&name) => {
                return Err(BatchError::at(
                    index,
                    format!(
                        "unknown op `{name}`; use one of {}",
                        GraphOp::NAMES.join(", ")
                    ),
                ));
            }
            Some(_) => {}
        }
        let op: GraphOp = serde_json::from_value(op)
            .map_err(|error| BatchError::at(index, error.to_string()))?;
        ops.push(op);
    }
    let batch = GraphEditBatch {
        project: raw.project,
        ops,
    };
    check_batch(&batch, known_refs)?;
    Ok(batch)
}

/// The checks of [`validate_batch`] on a typed batch: refs unique within the batch, every
/// `@<ref>` created earlier in the batch or in `known_refs`, and a non-empty goal on every create
/// and retitle. A create whose ref is in `known_refs` is a rebuild, not a duplicate.
pub fn check_batch(
    batch: &GraphEditBatch,
    known_refs: &BTreeMap<String, TaskId>,
) -> Result<(), BatchError> {
    let mut created: BTreeSet<&str> = BTreeSet::new();
    for (index, op) in batch.ops.iter().enumerate() {
        for task in op.task_refs() {
            if let TaskRef::Ref(name) = task
                && !created.contains(name.as_str())
                && !known_refs.contains_key(name)
            {
                return Err(BatchError::at(index, format!("unknown ref `@{name}`")));
            }
        }
        match op {
            GraphOp::Create {
                reference, goal, ..
            } => {
                if goal.trim().is_empty() {
                    return Err(BatchError::at(index, "create needs a goal"));
                }
                if let Some(name) = reference {
                    if name.is_empty() || name.starts_with('@') {
                        return Err(BatchError::at(
                            index,
                            format!("`{name}` is not a ref name"),
                        ));
                    }
                    if !created.insert(name) {
                        return Err(BatchError::at(index, format!("duplicate ref `{name}`")));
                    }
                }
            }
            GraphOp::Set {
                goal: Some(goal), ..
            } if goal.trim().is_empty() => {
                return Err(BatchError::at(index, "set needs a non-empty goal"));
            }
            _ => {}
        }
    }
    Ok(())
}
