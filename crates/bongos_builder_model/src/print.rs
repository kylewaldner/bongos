//! The printer: a task subtree as a proposal file, in the grammar of [`crate::parse`].

use std::collections::BTreeSet;

use bongos_work_model::commands::default_unblock;
use bongos_work_model::derive::{self, State};
use bongos_work_model::graph::child_order;
use bongos_work_model::layout::PROPOSALS_DIR;
use bongos_work_model::rules::{EdgeKind, role_name};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{TaskId, TodoState, UnblockPoint};

use crate::grammar::{kind_name, unblock_name};

/// The first line of every printed proposal.
pub const COMMENT: &str = "# bongos task proposal. Lines starting with # are comments. \
    The format is in ~/.bongos/task-builder/format.txt";

/// The work that is running now. Later steps define where it comes from; the printer only asks
/// which tasks have some.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveSet {
    /// The ids of tasks with a running chat, workflow run or worktree.
    pub running: BTreeSet<TaskId>,
}

impl LiveSet {
    fn touches(&self, task: &Task) -> bool {
        task.walk()
            .iter()
            .any(|descendant| descendant.id().is_some_and(|id| self.running.contains(id)))
    }
}

/// Prints the task `root_id` and its subtree as a proposal: the header (the next free number,
/// `for` the root, `by kyle`, an empty `why`), then the root and every descendant in child
/// order. Every task gets its `id`, `kind` and `state`; a todo its role, workflow and marks; an
/// edge its unblock point when that is not the default. A done or cancelled task below the root
/// prints as one line, unless some of its subtree is in `live`. The root always prints in full.
///
/// Returns `None` when `root_id` names no task of `tree` or that task never parsed.
pub fn print(tree: &WorkTree, root_id: &str, live: &LiveSet) -> Option<String> {
    let root = tree.task_by_id(root_id)?;
    root.file.as_ref()?;
    let mut lines = vec![
        COMMENT.to_string(),
        format!("proposal {}", next_number(root)),
        format!("for {root_id}"),
        "by kyle".to_string(),
        "why".to_string(),
        String::new(),
    ];
    write_task(&mut lines, tree, live, root, 0);
    let mut text = lines.join("\n");
    text.push('\n');
    Some(text)
}

/// One more than the highest proposal number of the task, recorded or on disk.
fn next_number(task: &Task) -> u32 {
    let recorded = task
        .file
        .iter()
        .flat_map(|file| file.proposals.iter().map(|record| record.number));
    let on_disk = task.files.keys().filter_map(|path| {
        if !path.starts_with(PROPOSALS_DIR) {
            return None;
        }
        path.file_name()?
            .to_str()?
            .strip_suffix(".proposal.txt")?
            .parse::<u32>()
            .ok()
    });
    recorded.chain(on_disk).max().unwrap_or(0) + 1
}

fn one_line(text: &str) -> String {
    text.split(['\r', '\n']).collect::<Vec<_>>().join(" ")
}

pub(crate) fn state_word(state: State) -> &'static str {
    match state {
        State::Cancelled => "cancelled",
        State::Done => "done",
        State::Merged => "merged",
        State::Blocked => "blocked",
        State::Ready => "ready",
        State::InProgress => "in-progress",
        State::Review => "review",
        State::Implemented => "implemented",
    }
}

fn write_task(lines: &mut Vec<String>, tree: &WorkTree, live: &LiveSet, task: &Task, depth: usize) {
    let Some(file) = &task.file else {
        return;
    };
    let indent = "  ".repeat(depth);
    let inner = "  ".repeat(depth + 1);
    let state = derive::state(task, tree);
    lines.push(format!(
        "{indent}task {}: {} | id {} | kind {} | state {}",
        task.slug,
        one_line(&file.goal),
        file.id,
        kind_name(file.kind),
        state_word(state),
    ));
    let collapsed =
        depth > 0 && matches!(state, State::Done | State::Cancelled) && !live.touches(task);
    if collapsed {
        return;
    }
    for edge in &file.after {
        lines.push(edge_line(&inner, EdgeKind::After, &edge.task, edge.unblock));
    }
    if let Some(edge) = &file.stacked_on {
        lines.push(edge_line(
            &inner,
            EdgeKind::StackedOn,
            &edge.task,
            edge.unblock,
        ));
    }
    if let Some(result) = &file.planned_result {
        lines.push(format!("{inner}result {}", one_line(result)));
    }
    for todo in &file.todos {
        let mut line = format!("{inner}todo {}", one_line(&todo.name));
        if let Some(role) = todo.role {
            line.push_str(&format!(" | role {}", role_name(role)));
        }
        if let Some(workflow) = &todo.workflow {
            line.push_str(&format!(" | workflow {}", one_line(workflow)));
        }
        match todo.state {
            TodoState::Open => {}
            TodoState::Done => line.push_str(" | done"),
            TodoState::Skipped => line.push_str(" | skipped"),
            TodoState::Cancelled => line.push_str(" | cancelled"),
        }
        if todo.started.is_some() {
            line.push_str(" | started");
        }
        lines.push(line);
    }
    for child in child_order(task) {
        write_task(lines, tree, live, child, depth + 1);
    }
}

fn edge_line(indent: &str, kind: EdgeKind, target: &str, unblock: UnblockPoint) -> String {
    let phrase = match kind {
        EdgeKind::After => "after",
        EdgeKind::StackedOn => "stacked on",
    };
    let mut line = format!("{indent}{phrase} {target}");
    if unblock != default_unblock(kind) {
        line.push_str(&format!(" | unblock {}", unblock_name(unblock)));
    }
    line
}
