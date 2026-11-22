//! Resolving a definition's inputs against the task a run starts on. Pure: it reads the loaded work
//! tree, never the disk.
//!
//! Sources: `task.goal`, `task.plan` (the task's `plan.md` when shared, else the nearest ancestor's
//! shared one), `task.artifacts` (the task's shared artifacts; a human-only artifact is never
//! listed), `task.pr`, `task.prs`, `task.base`, `task.branch`, `task.merge_is_ship`, `todo.name`
//! and `literal:<text>`. An override for an input's name wins over its source. A required input
//! with no value is `missing`.

use std::collections::BTreeMap;

use bongos_work_model::artifacts::task_artifacts;
use bongos_work_model::layout::PLAN_FILE;
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{TaskFile, Visibility};
use serde::{Deserialize, Serialize};

use crate::definition::Definition;

/// The source name an override is recorded under.
pub const OVERRIDE_SOURCE: &str = "override";

/// A resolved input value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    Text(String),
    List(Vec<String>),
    Bool(bool),
}

impl InputValue {
    /// The value as prompt text: a list is one item per line.
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::List(items) => items.join("\n"),
            Self::Bool(value) => value.to_string(),
        }
    }
}

/// One input of a run, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub name: String,
    pub value: Option<InputValue>,
    /// The definition's `from`, or [`OVERRIDE_SOURCE`].
    pub source: String,
    /// Required and without a value: the run cannot start.
    pub missing: bool,
}

/// An input whose source reads a task, resolved for a run with no task (a project subject).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", no_task_message(.input, .from))]
pub struct NoTaskInput {
    pub input: String,
    pub from: String,
}

/// Whether `source` reads the task or its todo (`task.*`, `todo.*`).
pub fn reads_task(source: &str) -> bool {
    source.starts_with("task.") || source.starts_with("todo.")
}

/// Why a run with no task cannot use the input `input` read from `source`.
pub fn no_task_message(input: &str, source: &str) -> String {
    format!("input {input} reads {source}; a run with no task cannot use it")
}

/// Resolves every input of `definition` for `task` and its todo at `todo` (an index into its
/// `todos`), in declaration order. With no task (a project subject) every input comes from
/// `overrides` or a `literal:` source; one that would read the task or a todo is an error.
pub fn resolve(
    definition: &Definition,
    task: Option<&Task>,
    tree: &WorkTree,
    todo: Option<usize>,
    overrides: &BTreeMap<String, InputValue>,
) -> Result<Vec<Input>, NoTaskInput> {
    definition
        .inputs
        .iter()
        .map(|input| {
            let (value, source) = match overrides.get(&input.name) {
                Some(value) => (Some(value.clone()), OVERRIDE_SOURCE.to_string()),
                None => {
                    let value = match task {
                        Some(task) => source_value(&input.from, task, tree, todo),
                        None if reads_task(&input.from) => {
                            return Err(NoTaskInput {
                                input: input.name.clone(),
                                from: input.from.clone(),
                            });
                        }
                        None => literal(&input.from),
                    };
                    (value, input.from.clone())
                }
            };
            Ok(Input {
                name: input.name.clone(),
                missing: input.required && value.is_none(),
                value,
                source,
            })
        })
        .collect()
}

fn literal(source: &str) -> Option<InputValue> {
    source
        .strip_prefix("literal:")
        .map(|text| InputValue::Text(text.to_string()))
}

/// The value of one source. Empty text and empty lists are no value. An unknown source has no
/// value (validation reports it).
pub fn source_value(
    source: &str,
    task: &Task,
    tree: &WorkTree,
    todo: Option<usize>,
) -> Option<InputValue> {
    if let Some(value) = literal(source) {
        return Some(value);
    }
    let file = task.file.as_ref();
    let text = |value: Option<&str>| {
        value
            .filter(|value| !value.is_empty())
            .map(|value| InputValue::Text(value.to_string()))
    };
    let field = |get: fn(&TaskFile) -> Option<&str>| text(file.and_then(get));
    match source {
        "task.goal" => text(Some(task.goal())),
        "task.plan" => plan(task, tree).map(InputValue::Text),
        "task.artifacts" => list(shared_artifact_paths(task)),
        "task.pr" => field(|file| file.result.as_ref()?.url.as_deref()),
        "task.prs" => {
            let result = file.and_then(|file| file.result.as_ref())?;
            if result.urls.is_empty() {
                list(result.url.iter().cloned().collect())
            } else {
                list(result.urls.clone())
            }
        }
        "task.base" => field(|file| file.base.as_deref()),
        "task.branch" => field(|file| file.branch.as_deref()),
        "task.merge_is_ship" => file
            .and_then(|file| file.merge_is_ship)
            .map(InputValue::Bool),
        "todo.name" => text(
            todo.and_then(|index| file?.todos.get(index))
                .map(|todo| todo.name.as_str()),
        ),
        _ => None,
    }
}

fn list(items: Vec<String>) -> Option<InputValue> {
    (!items.is_empty()).then_some(InputValue::List(items))
}

/// The absolute paths of `task`'s shared artifacts. Human-only artifacts are never in the list.
pub fn shared_artifact_paths(task: &Task) -> Vec<String> {
    task_artifacts(task)
        .artifacts
        .into_iter()
        .filter(|artifact| artifact.visibility == Some(Visibility::Shared))
        .map(|artifact| task.dir.join(&artifact.path).to_string_lossy().into_owned())
        .collect()
}

fn shares_plan(task: &Task) -> bool {
    task_artifacts(task).artifacts.iter().any(|artifact| {
        artifact.path == PLAN_FILE && artifact.visibility == Some(Visibility::Shared)
    })
}

/// The path of the shared `plan.md` of `task` or its nearest ancestor that has one.
fn plan(task: &Task, tree: &WorkTree) -> Option<String> {
    let mut current = Some(task);
    while let Some(task) = current {
        if shares_plan(task) {
            return Some(task.dir.join(PLAN_FILE).to_string_lossy().into_owned());
        }
        current = parent(task, tree);
    }
    None
}

/// The task above `task` in the tree: right for both folder layouts, since it follows `children`
/// and not the folder a task is in.
fn parent<'a>(task: &Task, tree: &'a WorkTree) -> Option<&'a Task> {
    tree.parent_of(task.id()?)
}
