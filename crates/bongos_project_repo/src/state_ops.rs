//! State operations: the state part of a change, kept as operations rather than rows because a
//! landing can wait while the engine keeps writing state. Each op runs against the row as it is
//! when it is applied.

use anyhow::{Context as _, Result, bail};
use bongos_project_repo_model::{TaskStateRecord, TodoStateRecord};
use bongos_work_model::{HistoryEntry, ProposalRecord, TaskId, TodoState};
use serde::{Deserialize, Serialize};

use crate::state::StateTx;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StateOp {
    /// Moves a todo's state, whatever it is when applied, from one task to another.
    MoveTodo {
        from_task: TaskId,
        to_task: TaskId,
        todo_id: String,
    },
    /// Moves the work of a task (branch, base, base SHA, rebase acknowledgement, worktree,
    /// result and ship override), whatever it is when applied.
    MoveWork {
        from_task: TaskId,
        to_task: TaskId,
    },
    /// Sets one field of a task's state record, only if it still holds `expected`; `None` is an
    /// absent field. Only the fields in [`SETTABLE_FIELDS`] can be set: todos and history have
    /// their own ops.
    Set {
        task: TaskId,
        field: String,
        expected: Option<toml::Value>,
        value: Option<toml::Value>,
    },
    /// Sets one todo's state, matched by its id, only if it still holds `expected`. A todo with
    /// no record, and an `expected` of `None`, read as open and never started.
    SetTodo {
        task: TaskId,
        expected: Option<TodoStateRecord>,
        value: TodoStateRecord,
    },
    AppendHistory {
        task: TaskId,
        entry: HistoryEntry,
    },
    /// Removes every state row of a task.
    Delete {
        task: TaskId,
    },
    /// Records a settled proposal on the root task, unless a record with its number is there.
    RecordProposal {
        root: TaskId,
        record: ProposalRecord,
    },
}

/// The fields of a task's state record that [`StateOp::Set`] may set.
pub const SETTABLE_FIELDS: [&str; 13] = [
    "status",
    "closed",
    "linear",
    "linear_state",
    "branch",
    "base",
    "base_sha",
    "rebase_ack_sha",
    "worktree",
    "ship_overridden",
    "result",
    "default_visibility",
    "proposals",
];

/// Applies `ops` in order. Returns a note for each op that kept a newer value.
pub(crate) fn apply(tx: &mut StateTx, ops: &[StateOp]) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for op in ops {
        apply_one(tx, op, &mut notes)?;
    }
    Ok(notes)
}

fn apply_one(tx: &mut StateTx, op: &StateOp, notes: &mut Vec<String>) -> Result<()> {
    match op {
        StateOp::MoveTodo {
            from_task,
            to_task,
            todo_id,
        } => {
            if from_task == to_task {
                return Ok(());
            }
            let mut from = tx.task(from_task)?.unwrap_or_default();
            let Some(position) = from
                .todos
                .iter()
                .position(|todo| todo.id.as_deref() == Some(todo_id.as_str()))
            else {
                notes.push(format!("todo {todo_id} is no longer in {from_task}"));
                return Ok(());
            };
            let todo = from.todos.remove(position);
            let mut to = tx.task(to_task)?.unwrap_or_default();
            to.todos
                .retain(|existing| existing.id.as_deref() != Some(todo_id.as_str()));
            to.todos.push(todo);
            tx.set_task(from_task, from)?;
            tx.set_task(to_task, to)?;
        }
        StateOp::MoveWork { from_task, to_task } => {
            if from_task == to_task {
                return Ok(());
            }
            let Some(mut from) = tx.task(from_task)? else {
                notes.push(format!("{from_task} has no work to move"));
                return Ok(());
            };
            let mut to = tx.task(to_task)?.unwrap_or_default();
            to.branch = from.branch.take();
            to.base = from.base.take();
            to.base_sha = from.base_sha.take();
            to.rebase_ack_sha = from.rebase_ack_sha.take();
            to.worktree = from.worktree.take();
            to.result = from.result.take();
            to.ship_overridden = from.ship_overridden.take();
            tx.set_task(from_task, from)?;
            tx.set_task(to_task, to)?;
        }
        StateOp::Set {
            task,
            field,
            expected,
            value,
        } => {
            if !SETTABLE_FIELDS.contains(&field.as_str()) {
                bail!("{field} is not a state field that can be set (task {task})");
            }
            let current = tx.task(task)?.unwrap_or_default();
            let toml::Value::Table(mut table) = toml::Value::try_from(&current)
                .with_context(|| format!("reading the state of {task}"))?
            else {
                bail!("the state of {task} is not a table");
            };
            if table.get(field) != expected.as_ref() {
                notes.push(format!("kept newer {field} of {task}"));
                return Ok(());
            }
            match value {
                Some(value) => {
                    table.insert(field.clone(), value.clone());
                }
                None => {
                    table.remove(field);
                }
            }
            let record: TaskStateRecord = toml::Value::Table(table)
                .try_into()
                .with_context(|| format!("setting {field} of {task}"))?;
            tx.set_task(task, record)?;
        }
        StateOp::SetTodo {
            task,
            expected,
            value,
        } => {
            let Some(todo_id) = value.id.as_deref() else {
                bail!("a todo state set alone needs a todo id (task {task})");
            };
            let current = tx.task(task)?.unwrap_or_default();
            // A todo with no record reads as open and never started.
            let open = TodoStateRecord {
                id: Some(todo_id.to_string()),
                state: TodoState::Open,
                started: None,
            };
            let existing = current
                .todos
                .iter()
                .find(|todo| todo.id.as_deref() == Some(todo_id))
                .unwrap_or(&open);
            if existing != expected.as_ref().unwrap_or(&open) {
                notes.push(format!("kept newer state of todo {todo_id} of {task}"));
                return Ok(());
            }
            tx.set_todo(task, value.clone())?;
        }
        StateOp::AppendHistory { task, entry } => {
            tx.append_history(task, entry.clone())?;
        }
        StateOp::Delete { task } => {
            tx.delete_task(task)?;
        }
        StateOp::RecordProposal { root, record } => {
            let mut state = tx.task(root)?.unwrap_or_default();
            if state
                .proposals
                .iter()
                .any(|existing| existing.number == record.number)
            {
                notes.push(format!("kept newer proposal {} of {root}", record.number));
                return Ok(());
            }
            state.proposals.push(record.clone());
            tx.set_task(root, state)?;
        }
    }
    Ok(())
}
