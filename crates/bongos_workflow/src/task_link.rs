//! How a run meets its task (feature 20, D6 and D15): what a node may learn about the task, the
//! todo a run starts and completes, the history it leaves, and the end of a successful run.
//!
//! - What reaches a node: the inputs (`inputs::resolve`, which never lists a human-only
//!   artifact), prompts rendered from them, the state document built from them, and the system
//!   file rendered from 03's `session_context`, whose only artifacts are the shared ones. Nothing
//!   here reads the task directory for itself.
//! - The todo: a start marks it started (`todo: start`, unless it already is) and records
//!   `run started` by `workflow:<name>`; success completes exactly the run's todo through 03's
//!   `advance` on that todo, in one write; failure and cancellation only record history, leaving
//!   the todo open. Nothing else about the task's todos changes.
//! - The end of a successful run: the scheduler's succeeded end is held for the engine
//!   ([`crate::runtime::Runtime::hold_success`]), which walks [`next`]: the final merge gate when
//!   the definition has no merge node, the deploy confirmation for a ship todo whose merge is not
//!   its ship, the outcomes, then the todo. A wait is a gate on a key of no node, so Kyle approves
//!   it from the run like any other gate.

use std::fmt::Write as _;
use std::sync::Arc;

use bongos_work::WriteError;
use bongos_work_model::commands::{self, Change, Context, Outcome};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Extra, HistoryEntry, SessionContext, TodoRole, TodoState};
use bongos_workflow_model::definition::NodeKind;
use bongos_workflow_model::inputs::Input;
use bongos_workflow_model::record::TodoRef;
use bongos_workflow_model::template::{Scope, TemplateError, render};
use bongos_workflow_model::{Definition, Failure, NodeKey, RunState};
use gpui::App;

use crate::builtin_wait::WaitCondition;

/// The prefix of the `by` of everything a workflow writes on a task.
pub const BY_WORKFLOW: &str = "workflow:";
/// The gate of the final merge, for a definition with no merge node.
pub const MERGE_GATE: &str = "@merge";
/// The deploy confirmation of a ship todo whose merge is not its ship.
pub const DEPLOY_GATE: &str = "@deploy";
/// The wait for a todo that moved or was done by hand.
pub const TODO_GATE: &str = "@todo";
/// The wait for a completion 03 refused (the ships rule of a stacked leaf, say).
pub const REFUSED_GATE: &str = "@refused";
pub const MERGE_QUESTION: &str = "Merge the run into the task branch?";
pub const DEPLOY_QUESTION: &str = "Merge is not ship for this task: confirm the deploy to \
                                   complete the ship todo?";
pub const TODO_MOVED: &str = "todo moved or done by hand";
/// The history events of a run on its task.
pub const RUN_STARTED: &str = "run started";
pub const RUN_FAILED: &str = "run failed";
pub const RUN_CANCELLED: &str = "run cancelled";

/// `workflow:<name>`.
pub fn by(workflow: &str) -> String {
    format!("{BY_WORKFLOW}{workflow}")
}

/// The key of one of the engine's own gates.
pub fn gate_key(gate: &str) -> NodeKey {
    NodeKey::new(gate, 1)
}

/// Whether `key` is one of the engine's own gates rather than a node's.
pub fn is_engine_gate(key: &NodeKey) -> bool {
    key.node.starts_with('@')
}

/// Whether Kyle approved the engine gate `gate` (it closed and was not opened again).
pub fn approved(state: &RunState, gate: &str) -> bool {
    let key = gate_key(gate);
    !state.open_gates.contains_key(&key) && state.closed_gates.iter().any(|closed| closed.key == key)
}

/// The values a prompt may name: `inputs.<name>` for every input with a value.
pub fn prompt_scope(inputs: &[Input]) -> Scope {
    let mut scope = Scope::new();
    for input in inputs {
        if let Some(value) = &input.value {
            scope.set(format!("inputs.{}", input.name), value.to_text());
        }
    }
    scope
}

/// `text` rendered with the run's inputs ([`prompt_scope`]).
pub fn render_prompt(text: &str, inputs: &[Input]) -> Result<String, TemplateError> {
    render(text, &prompt_scope(inputs))
}

/// The system file an agent node gets (D6): the task context of `session_context`, the same set
/// feature 04 gives chats. The only artifacts in it are `context.artifacts`, the shared ones.
pub fn system_text(context: &SessionContext, workflow: &str, todo: &TodoRef) -> String {
    let line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    let _ = writeln!(out, "# Task context\n");
    let _ = writeln!(out, "Goal: {}", line(&context.goal));
    let _ = writeln!(out, "Task file: {}", context.task_toml.display());
    let _ = writeln!(out, "Task directory: {}", context.task_dir.display());
    if let Some(worktree) = &context.worktree {
        let _ = writeln!(out, "Task worktree: {}", worktree.display());
    }
    if let Some(group) = &context.parent_goal {
        let _ = writeln!(out, "Group goal: {}", line(group));
    }
    let _ = writeln!(out, "Workflow: {workflow}");
    let _ = writeln!(out, "Todo: {}", line(&todo.name));
    if !context.artifacts.is_empty() {
        let _ = writeln!(out, "\n## Shared artifacts\n");
        for artifact in &context.artifacts {
            let description = line(&artifact.description);
            if description.is_empty() {
                let _ = writeln!(out, "- {}", artifact.path.display());
            } else {
                let _ = writeln!(out, "- {}: {description}", artifact.path.display());
            }
        }
    }
    out
}

/// The index of the run's todo in `task` now: `todo.index` when it still has `todo.name` and is
/// open, else the one open todo of that name, else `None` (moved or done by hand).
pub fn locate_todo(task: &Task, todo: &TodoRef) -> Option<usize> {
    let todos = &task.file.as_ref()?.todos;
    if todos
        .get(todo.index)
        .is_some_and(|found| found.name == todo.name && found.state == TodoState::Open)
    {
        return Some(todo.index);
    }
    let mut open = todos
        .iter()
        .enumerate()
        .filter(|(_, found)| found.name == todo.name && found.state == TodoState::Open);
    match (open.next(), open.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

fn entry(ctx: &Context, event: &str, detail: Option<String>) -> HistoryEntry {
    HistoryEntry {
        at: ctx.at,
        by: ctx.by.clone(),
        event: event.to_string(),
        detail,
        extra: Extra::new(),
    }
}

/// The start of a run of `workflow` on the todo at `index`: the todo marked started (unless it
/// already is) and `run started: <workflow>` in the history, in one change.
pub fn start_change(
    tree: &WorkTree,
    ctx: &Context,
    task_id: &str,
    index: usize,
    workflow: &str,
) -> Outcome {
    let already = tree
        .task_by_id(task_id)
        .and_then(|task| task.file.as_ref())
        .and_then(|file| file.todos.get(index))
        .is_some_and(|todo| todo.started.is_some());
    if already {
        return commands::record_event(tree, ctx, task_id, RUN_STARTED, Some(workflow.to_string()));
    }
    let mut change = commands::todo_start(tree, ctx, task_id, Some(index))?;
    let started = entry(ctx, RUN_STARTED, Some(workflow.to_string()));
    let dir = change.dir.clone();
    for write in &mut change.writes {
        if let commands::Write::Task { dir: written, file } = write
            && *written == dir
        {
            file.history.push(started.clone());
        }
    }
    change.entry = started;
    Ok(change)
}

/// The completion of the run's todo: 03's `advance` on exactly that todo ([`locate_todo`]),
/// refused with [`TODO_MOVED`] when it moved or was done, or with 03's own reason (the ships rule
/// of a stacked leaf) when that refuses.
pub fn complete_change(tree: &WorkTree, ctx: &Context, task_id: &str, todo: &TodoRef) -> Outcome {
    let task = tree
        .task_by_id(task_id)
        .ok_or_else(|| format!("no task {task_id} is loaded"))?;
    let index = locate_todo(task, todo).ok_or_else(|| TODO_MOVED.to_string())?;
    commands::advance_todo(tree, ctx, task_id, Some(index), false)
}

/// `run failed: <reason> at <node>` in the history; the todo stays open.
pub fn failed_change(
    tree: &WorkTree,
    ctx: &Context,
    task_id: &str,
    failure: Option<&Failure>,
) -> Outcome {
    commands::record_event(tree, ctx, task_id, RUN_FAILED, Some(failure_detail(failure)))
}

/// `<reason> at <node>`, or the reason alone when no node failed.
pub fn failure_detail(failure: Option<&Failure>) -> String {
    let Some(failure) = failure else {
        return "unknown reason".to_string();
    };
    match &failure.node {
        Some(key) => format!("{} at {}", failure.reason.label(), key.node),
        None => failure.reason.label().to_string(),
    }
}

/// `run cancelled` in the history; the todo stays open.
pub fn cancelled_change(tree: &WorkTree, ctx: &Context, task_id: &str) -> Outcome {
    commands::record_event(tree, ctx, task_id, RUN_CANCELLED, None)
}

/// What the end of a successful run must still do, read from its definition and task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finale {
    /// The definition has a `merge` node, so the engine offers no final merge.
    pub merge_node: bool,
    /// The definition waits on `deploy`.
    pub deploy_wait: bool,
    /// The run's todo is the task's ships todo.
    pub ship_todo: bool,
    /// `bongos_work::merge_is_ship` for the task.
    pub merge_is_ship: bool,
}

impl Finale {
    pub fn read(definition: &Definition, task: Option<&Task>, todo: &TodoRef, merge_is_ship: bool) -> Self {
        let uses = |name: &str| {
            definition.nodes.iter().filter(move |node| {
                node.kind.value == NodeKind::Program
                    && node.use_node.as_ref().is_some_and(|used| used.value == name)
            })
        };
        let deploy_wait = uses("wait").any(|node| {
            node.with
                .get("condition")
                .and_then(|value| value.as_str())
                .and_then(WaitCondition::parse)
                == Some(WaitCondition::Deploy)
        });
        let ship_todo = task
            .and_then(|task| task.file.as_ref())
            .and_then(|file| {
                let index = task.and_then(|task| locate_todo(task, todo)).unwrap_or(todo.index);
                file.todos.get(index)
            })
            .is_some_and(|found| found.role == Some(TodoRole::Ships));
        Self {
            merge_node: uses("merge").next().is_some(),
            deploy_wait,
            ship_todo,
            merge_is_ship,
        }
    }

    /// A ship todo whose merge is not its ship completes only after a confirmed deploy: the
    /// definition's `deploy` wait, else the engine's own confirmation.
    pub fn needs_deploy_gate(&self) -> bool {
        self.ship_todo && !self.merge_is_ship && !self.deploy_wait
    }
}

/// The next thing the end of a successful run does.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Wait on Kyle at the engine gate `key`.
    Wait { key: NodeKey, question: String },
    /// Merge the run branch into the task branch (Kyle approved the final merge).
    Merge,
    /// Deliver the outcomes.
    Deliver,
    /// Complete the run's todo, then end succeeded.
    CompleteTodo,
}

/// Where the end of a successful run stands: `merged` once the final merge is recorded,
/// `delivered` once the outcomes are.
pub fn next(finale: &Finale, state: &RunState, merged: bool, delivered: bool) -> Next {
    if !finale.merge_node && !merged {
        if approved(state, MERGE_GATE) {
            return Next::Merge;
        }
        return Next::Wait {
            key: gate_key(MERGE_GATE),
            question: MERGE_QUESTION.to_string(),
        };
    }
    if finale.needs_deploy_gate() && !approved(state, DEPLOY_GATE) {
        return Next::Wait {
            key: gate_key(DEPLOY_GATE),
            question: DEPLOY_QUESTION.to_string(),
        };
    }
    if !delivered {
        return Next::Deliver;
    }
    Next::CompleteTodo
}

/// What the run's todo completion ends in, from the work store's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Completed {
    /// The todo is done: the run ends succeeded.
    Done,
    /// Waits on Kyle at this engine gate with this question.
    Wait { key: NodeKey, question: String },
}

/// The run's answer to the completion write: done; a wait "todo moved or done by hand" (once Kyle
/// approved that wait, the run ends succeeded without changing any todo); or a wait with the
/// reason 03 refused.
pub fn completed(state: &RunState, result: &Result<Change, WriteError>) -> Completed {
    match result {
        Ok(_) => Completed::Done,
        Err(WriteError::Refused(reason)) if reason == TODO_MOVED => {
            if approved(state, TODO_GATE) {
                Completed::Done
            } else {
                Completed::Wait {
                    key: gate_key(TODO_GATE),
                    question: TODO_MOVED.to_string(),
                }
            }
        }
        Err(error) => Completed::Wait {
            key: gate_key(REFUSED_GATE),
            question: error.to_string(),
        },
    }
}

/// A command the engine hands to the work store.
pub type WriteCommand = Box<dyn Fn(&WorkTree, &Context) -> Outcome + Send + 'static>;

/// Where the engine reads the work tree and writes tasks: 03's [`bongos_work::WorkStore`] in the
/// app, through its serial writer; tests use their own.
pub trait TaskWrites: Send + Sync {
    /// The loaded tree.
    fn tree(&self, cx: &App) -> Option<Arc<WorkTree>>;
    /// Queues `command`; resolves once written.
    fn write(
        &self,
        ctx: Context,
        command: WriteCommand,
        cx: &mut App,
    ) -> gpui::Task<Result<Change, WriteError>>;
    /// `bongos_work::merge_is_ship` for `task`.
    fn merge_is_ship(&self, task: &Task, cx: &App) -> bool;
}

/// [`TaskWrites`] through the app's work store.
pub struct StoreWrites;

impl TaskWrites for StoreWrites {
    fn tree(&self, cx: &App) -> Option<Arc<WorkTree>> {
        bongos_work::WorkStore::global(cx).and_then(|store| store.read(cx).snapshot())
    }

    fn write(
        &self,
        ctx: Context,
        command: WriteCommand,
        cx: &mut App,
    ) -> gpui::Task<Result<Change, WriteError>> {
        match bongos_work::WorkStore::global(cx) {
            Some(store) => store.update(cx, |store, cx| store.write(ctx, command, cx)),
            None => gpui::Task::ready(Err(WriteError::Off)),
        }
    }

    fn merge_is_ship(&self, task: &Task, cx: &App) -> bool {
        bongos_work::merge_is_ship(task, cx)
    }
}
