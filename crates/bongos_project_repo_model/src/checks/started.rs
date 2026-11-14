//! Check 6, `started-unchanged`: a change leaves the shape of a started task alone. Its
//! `areas`, `work_kind`, `size`, edges, folder and presence stay; a todo with state is neither
//! removed nor reordered (added todos are fine). Two exemptions: a split moves todos into a new
//! child with matching staged `MoveTodo` ops, and a move's own coordinator changes may touch the
//! tasks its move holds. Nothing a caller writes (subject, body, notes, trailers) makes a change
//! part of a move.

use std::collections::BTreeMap;

use super::live::{HoldReason, LiveFacts};
use super::outcome::CheckOutcome;
use crate::change_record::ChangeMeta;
use crate::definitions::{DefinitionNode, DefinitionTree};
use crate::summary::{ChangeSummary, edges_of, nodes_by_id, parent_folder};

/// The marker of a move's trailers; text carrying it is stripped from a change's message.
pub const MOVE_MARKER: &str = "Bongos-Move";

/// A staged `MoveTodo` op of the change (a state op recorded with it and applied when it
/// lands): the todo `todo_id` moves from one task's state to another's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedTodoMove {
    pub from_task: String,
    pub to_task: String,
    pub todo_id: String,
}

/// What check 6 reads.
pub struct StartedInput<'a> {
    pub master: &'a DefinitionTree,
    pub new: &'a DefinitionTree,
    pub summary: &'a ChangeSummary,
    pub live: &'a LiveFacts,
    pub change_id: &'a str,
    pub meta: &'a ChangeMeta,
    pub staged_moves: &'a [StagedTodoMove],
}

/// Check 6.
pub fn started_unchanged(input: &StartedInput) -> CheckOutcome {
    let old = nodes_by_id(input.master);
    let now = nodes_by_id(input.new);
    let mut refusals = Vec::new();
    for (id, before) in &old {
        let hold = input.live.held(id);
        if hold
            .as_ref()
            .is_some_and(|hold| hold.exempts(input.change_id))
        {
            continue;
        }
        if !input.live.started(id) {
            continue;
        }
        let after = now.get(id).copied();
        let name = task_name(before);
        if let Some(hold) = hold {
            let touched = after.is_none_or(|after| {
                after.definition != before.definition || after.folder != before.folder
            });
            if touched {
                refusals.push(held_refusal(&name, &hold, input.meta));
            }
            continue;
        }
        let Some(after) = after else {
            refusals.push(format!("started task {name} was removed"));
            continue;
        };
        refusals.extend(shape_refusals(&name, before, after));
        refusals.extend(todo_refusals(&name, before, after, input, &now));
    }
    CheckOutcome::from_refusals(refusals)
}

fn task_name(node: &DefinitionNode) -> String {
    if node.slug.is_empty() {
        format!("the top group ({})", node.definition.id)
    } else {
        node.slug.clone()
    }
}

fn held_refusal(name: &str, hold: &HoldReason, meta: &ChangeMeta) -> String {
    let move_id = hold.move_id();
    let names_move = [&meta.subject, &meta.body, &meta.notes]
        .iter()
        .any(|text| !move_id.is_empty() && text.contains(move_id));
    if names_move {
        format!(
            "task {name} is held: {hold}; naming the move in a change does not make the change part of it"
        )
    } else {
        format!("task {name} is held: {hold}; only the move itself may change it")
    }
}

fn shape_refusals(name: &str, before: &DefinitionNode, after: &DefinitionNode) -> Vec<String> {
    let (a, b) = (&before.definition, &after.definition);
    let mut changed = Vec::new();
    if a.areas != b.areas {
        changed.push("areas");
    }
    if a.work_kind != b.work_kind {
        changed.push("work kind");
    }
    if a.size != b.size {
        changed.push("size");
    }
    if edges_of(before) != edges_of(after) {
        changed.push("edges");
    }
    if parent_folder(&before.folder) != parent_folder(&after.folder) || before.slug != after.slug {
        changed.push("folder");
    }
    changed
        .into_iter()
        .map(|what| format!("started task {name}: its {what} may not change"))
        .collect()
}

fn todo_refusals(
    name: &str,
    before: &DefinitionNode,
    after: &DefinitionNode,
    input: &StartedInput,
    now: &BTreeMap<&str, &DefinitionNode>,
) -> Vec<String> {
    let id = before.definition.id.as_str();
    let with_state = input.live.todos_with_state(id);
    let ids = |node: &DefinitionNode| -> Vec<String> {
        node.definition
            .todos
            .iter()
            .filter_map(|todo| todo.id.clone())
            .filter(|todo| with_state.contains(todo))
            .collect()
    };
    let old_order = ids(before);
    let new_order = ids(after);
    let mut refusals = Vec::new();
    for todo in &old_order {
        if new_order.contains(todo) || moved_by_split(id, after, todo, input, now) {
            continue;
        }
        let todo_name = before
            .definition
            .todos
            .iter()
            .find(|candidate| candidate.id.as_deref() == Some(todo.as_str()))
            .map_or(todo.as_str(), |candidate| candidate.name.as_str());
        refusals.push(format!(
            "started task {name}: todo {todo_name} has state and may not be removed"
        ));
    }
    let kept: Vec<&String> = old_order.iter().filter(|t| new_order.contains(t)).collect();
    let still: Vec<&String> = new_order.iter().filter(|t| old_order.contains(t)).collect();
    if kept != still {
        refusals.push(format!(
            "started task {name}: todos with state may not be reordered"
        ));
    }
    refusals
}

/// The split exemption: `todo` left the started task `from` for a child the change adds under
/// it, and the change's staged state moves it there.
fn moved_by_split(
    from: &str,
    after: &DefinitionNode,
    todo: &str,
    input: &StartedInput,
    now: &BTreeMap<&str, &DefinitionNode>,
) -> bool {
    input.summary.tasks_added.iter().any(|added| {
        let Some(child) = now.get(added.as_str()) else {
            return false;
        };
        parent_folder(&child.folder) == after.folder
            && !child.folder.is_empty()
            && child
                .definition
                .todos
                .iter()
                .any(|candidate| candidate.id.as_deref() == Some(todo))
            && input
                .staged_moves
                .iter()
                .any(|op| op.from_task == from && op.to_task == *added && op.todo_id == todo)
    })
}

/// `text` without any line naming a move's marker, so a caller's text never reads as a move's
/// trailer.
pub fn strip_move_text(text: &str) -> String {
    let marker = MOVE_MARKER.to_ascii_lowercase();
    text.lines()
        .filter(|line| !line.to_ascii_lowercase().contains(&marker))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `meta` with [`strip_move_text`] applied to its subject, body and notes.
pub fn strip_move_meta(meta: &ChangeMeta) -> ChangeMeta {
    ChangeMeta {
        subject: strip_move_text(&meta.subject),
        body: strip_move_text(&meta.body),
        notes: strip_move_text(&meta.notes),
        ..meta.clone()
    }
}
