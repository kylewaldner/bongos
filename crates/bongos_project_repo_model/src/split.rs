use bongos_work_model::{
    Edge, Extra, HistoryEntry, Kind, LinearState, ProposalRecord, SCHEMA, ShipOverridden,
    StackedOn, Status, TaskFile, TaskResult, Timestamp, Todo, TodoRole, TodoState, Visibility,
};
use serde::{Deserialize, Serialize};

/// Keys a definition file must not hold: they belong to the state store. A definition that
/// holds one is refused. The `todo.` keys are the `state` and `started` keys of a todo.
pub const RESERVED_STATE_KEYS: [&str; 14] = [
    "status",
    "closed",
    "todo.state",
    "todo.started",
    "result",
    "branch",
    "base",
    "base_sha",
    "worktree",
    "claims",
    "history",
    "linear",
    "linear_state",
    "proposals",
];

fn schema_one() -> u32 {
    SCHEMA
}

/// The part of a `task.toml` that travels in the repository. Keys it has no field for, including
/// the reserved state keys, land in `extra` so nothing is dropped silently; see
/// [`reserved_keys_in`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskDefinition {
    #[serde(default = "schema_one")]
    pub schema: u32,
    pub id: String,
    pub goal: String,
    pub kind: Kind,
    pub created: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_is_ship: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<Edge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacked_on: Option<StackedOn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_result: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The definition of one todo: everything but its state and start time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TodoDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<TodoRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The part of a `task.toml` that stays on one machine, in the state store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskStateRecord {
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear_state: Option<LinearState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebase_ack_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ship_overridden: Option<ShipOverridden>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    #[serde(default)]
    pub default_visibility: Visibility,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<ProposalRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryEntry>,
    /// The state of the task's todos, keyed by todo id so renaming or moving a todo keeps it. A
    /// todo without an id (an old-format file) matches the record at its position.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoStateRecord>,
}

/// The state and start time of one todo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TodoStateRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub state: TodoState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<Timestamp>,
}

/// Divides a task file into its definition and its state. `join(split(file))` is `file`.
pub fn split(file: &TaskFile) -> (TaskDefinition, TaskStateRecord) {
    // Every field is named so that a new `TaskFile` field fails to compile here until it is
    // classified.
    let TaskFile {
        schema,
        id,
        goal,
        kind,
        status,
        created,
        closed,
        repository,
        work_kind,
        areas,
        size,
        repositories,
        merge_is_ship,
        default_visibility,
        activity,
        linear,
        branch,
        base,
        base_sha,
        rebase_ack_sha,
        worktree,
        ship_overridden,
        after,
        stacked_on,
        todos,
        result,
        planned_result,
        proposals,
        linear_state,
        history,
        extra,
    } = file.clone();

    let mut todo_definitions = Vec::with_capacity(todos.len());
    let mut todo_states = Vec::with_capacity(todos.len());
    for todo in todos {
        let Todo {
            id,
            name,
            role,
            workflow,
            state,
            started,
            extra,
        } = todo;
        todo_states.push(TodoStateRecord {
            id: id.clone(),
            state,
            started,
        });
        todo_definitions.push(TodoDefinition {
            id,
            name,
            role,
            workflow,
            extra,
        });
    }

    let definition = TaskDefinition {
        schema,
        id,
        goal,
        kind,
        created,
        repository,
        work_kind,
        areas,
        size,
        repositories,
        merge_is_ship,
        activity,
        after,
        stacked_on,
        todos: todo_definitions,
        planned_result,
        extra,
    };
    let state = TaskStateRecord {
        status,
        closed,
        linear,
        linear_state,
        branch,
        base,
        base_sha,
        rebase_ack_sha,
        worktree,
        ship_overridden,
        result,
        default_visibility,
        proposals,
        history,
        todos: todo_states,
    };
    (definition, state)
}

/// Combines a definition and a state into a task file. A todo takes the state record with its
/// id, or the record at its position when it has no id; a todo with no record is open.
pub fn join(definition: TaskDefinition, state: TaskStateRecord) -> TaskFile {
    let TaskDefinition {
        schema,
        id,
        goal,
        kind,
        created,
        repository,
        work_kind,
        areas,
        size,
        repositories,
        merge_is_ship,
        activity,
        after,
        stacked_on,
        todos,
        planned_result,
        extra,
    } = definition;
    let TaskStateRecord {
        status,
        closed,
        linear,
        linear_state,
        branch,
        base,
        base_sha,
        rebase_ack_sha,
        worktree,
        ship_overridden,
        result,
        default_visibility,
        proposals,
        history,
        todos: todo_states,
    } = state;

    let todos = todos
        .into_iter()
        .enumerate()
        .map(|(index, todo)| {
            let record = match &todo.id {
                Some(id) => todo_states
                    .iter()
                    .find(|record| record.id.as_ref() == Some(id)),
                None => todo_states.get(index).filter(|record| record.id.is_none()),
            };
            Todo {
                id: todo.id,
                name: todo.name,
                role: todo.role,
                workflow: todo.workflow,
                state: record.map_or(TodoState::Open, |record| record.state),
                started: record.and_then(|record| record.started),
                extra: todo.extra,
            }
        })
        .collect();

    TaskFile {
        schema,
        id,
        goal,
        kind,
        status,
        created,
        closed,
        repository,
        work_kind,
        areas,
        size,
        repositories,
        merge_is_ship,
        default_visibility,
        activity,
        linear,
        branch,
        base,
        base_sha,
        rebase_ack_sha,
        worktree,
        ship_overridden,
        after,
        stacked_on,
        todos,
        result,
        planned_result,
        proposals,
        linear_state,
        history,
        extra,
    }
}

/// The reserved state keys a definition holds, in the order of [`RESERVED_STATE_KEYS`], each
/// once. A todo's `state` and `started` are reported as `todo.state` and `todo.started`.
pub fn reserved_keys_in(definition: &TaskDefinition) -> Vec<&'static str> {
    RESERVED_STATE_KEYS
        .into_iter()
        .filter(|key| match key.strip_prefix("todo.") {
            Some(todo_key) => definition
                .todos
                .iter()
                .any(|todo| todo.extra.contains_key(todo_key)),
            None => definition.extra.contains_key(*key),
        })
        .collect()
}
