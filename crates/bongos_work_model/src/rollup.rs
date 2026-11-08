//! What a group shows on its row: progress, the blocked count and where the owner is needed. Pure
//! functions over the loaded tree; nothing here is stored.

use std::collections::HashSet;

use crate::derive::{self, Shape, State};
use crate::graph::child_order;
use crate::store::{Task, WorkTree};
use crate::types::TaskId;

/// The roll-up of one group's subtree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rollup {
    /// Leaves that are Done.
    pub done: usize,
    /// Leaves that are not Cancelled. Groups are not counted.
    pub total: usize,
    /// Blocked leaves, minus those held back only by tasks that are already moving.
    pub blocked: usize,
    /// Tasks in the subtree (the group included) that are in the needs-you set.
    pub needs_you: usize,
    /// The drill path to the first needs-you task in display order: the ids from the group down to
    /// that task, both ends included. `None` when nothing in the subtree needs the owner.
    pub first_needs_you: Option<Vec<TaskId>>,
}

impl Rollup {
    /// The progress as shown on a row, such as `4/12`.
    pub fn progress_text(&self) -> String {
        format!("{}/{}", self.done, self.total)
    }
}

/// Walks the subtree of `group` once. A group that is Cancelled contributes nothing.
///
/// A Blocked leaf counts toward `blocked` unless every edge holding it back (its own and its
/// ancestors') points at a task that is moving: In Progress, Implemented or Review. Because the
/// walk covers the whole subtree, a needs-you task reaches the roll-up of every ancestor.
pub fn rollup(group: &Task, tree: &WorkTree, needs_you: &HashSet<TaskId>) -> Rollup {
    let mut rollup = Rollup::default();
    let mut path = Vec::new();
    walk(group, tree, needs_you, &mut path, &mut rollup);
    rollup
}

fn walk(
    task: &Task,
    tree: &WorkTree,
    needs_you: &HashSet<TaskId>,
    path: &mut Vec<TaskId>,
    rollup: &mut Rollup,
) {
    let state = derive::state(task, tree);
    if state == State::Cancelled {
        return;
    }
    let id = task.id();
    if let Some(id) = id {
        path.push(id.to_string());
        if needs_you.contains(id) {
            rollup.needs_you += 1;
            if rollup.first_needs_you.is_none() {
                rollup.first_needs_you = Some(path.clone());
            }
        }
    }
    match derive::shape(task, tree) {
        Shape::Group => {
            for child in child_order(task) {
                walk(child, tree, needs_you, path, rollup);
            }
        }
        Shape::Leaf => {
            rollup.total += 1;
            match state {
                State::Done => rollup.done += 1,
                State::Blocked if counts_as_blocked(task, tree) => rollup.blocked += 1,
                _ => {}
            }
        }
    }
    if id.is_some() {
        path.pop();
    }
}

fn counts_as_blocked(leaf: &Task, tree: &WorkTree) -> bool {
    let targets = derive::blocking_targets(leaf, tree);
    targets.is_empty() || !targets.iter().all(|target| is_moving(target, tree))
}

fn is_moving(task: &Task, tree: &WorkTree) -> bool {
    matches!(
        derive::state(task, tree),
        State::InProgress | State::Implemented | State::Review
    )
}
