//! The order and the picture of one level of the task tree: siblings as boxes, their `after` and
//! `stacked on` edges as labelled arrows. Pure functions; the text is plain ASCII and the same for
//! the same tree.

use std::collections::HashSet;

use crate::activity::created_at;
use crate::derive::{self, State};
use crate::store::{Task, WorkTree};
use crate::types::{TaskId, UnblockPoint};

/// One line of the level graph. Lines of a task's box carry its id, so RET on any of them drills
/// into it; arrow lines carry none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphLine {
    pub text: String,
    pub node: Option<TaskId>,
}

#[derive(Clone, Copy)]
enum EdgeKind {
    After,
    Stacked,
}

impl EdgeKind {
    fn label(self) -> &'static str {
        match self {
            EdgeKind::After => "after",
            EdgeKind::Stacked => "stacked",
        }
    }
}

/// The `(kind, target slug, unblock point)` of every edge on `task`: `after` edges in file
/// order, then the `stacked_on` edge.
fn edges(task: &Task) -> Vec<(EdgeKind, &str, UnblockPoint)> {
    let Some(file) = &task.file else {
        return Vec::new();
    };
    let mut edges: Vec<_> = file
        .after
        .iter()
        .map(|edge| (EdgeKind::After, edge.task.as_str(), edge.unblock))
        .collect();
    if let Some(edge) = &file.stacked_on {
        edges.push((EdgeKind::Stacked, edge.task.as_str(), edge.unblock));
    }
    edges
}

/// The children of `group` in dependency order: a topological order over `after` and `stacked on`
/// edges among the siblings, ties broken by `created` (then slug), so the common case is one
/// linear list. An edge naming no sibling, or the task itself, is ignored. If edges form a cycle
/// the tasks left over are appended by `created`; this never panics.
pub fn child_order(group: &Task) -> Vec<&Task> {
    let kids = &group.children;
    let mut by_created: Vec<usize> = (0..kids.len()).collect();
    by_created.sort_by_key(|&index| {
        kids.get(index)
            .map(|kid| (created_at(kid), kid.slug.clone()))
    });
    let predecessors: Vec<Vec<usize>> = kids
        .iter()
        .enumerate()
        .map(|(index, kid)| {
            edges(kid)
                .into_iter()
                .filter_map(|(_, slug, _)| kids.iter().position(|other| other.slug == slug))
                .filter(|&other| other != index)
                .collect()
        })
        .collect();
    let mut placed: HashSet<usize> = HashSet::new();
    let mut order = Vec::new();
    while let Some(&next) = by_created.iter().find(|&&index| {
        !placed.contains(&index)
            && predecessors
                .get(index)
                .is_some_and(|before| before.iter().all(|other| placed.contains(other)))
    }) {
        placed.insert(next);
        order.push(next);
    }
    order.extend(by_created.iter().filter(|index| !placed.contains(index)));
    order
        .into_iter()
        .filter_map(|index| kids.get(index))
        .collect()
}

/// The badge text for a state.
pub fn state_label(state: State) -> &'static str {
    match state {
        State::Cancelled => "Cancelled",
        State::Done => "Done",
        State::Merged => "Merged",
        State::Blocked => "Blocked",
        State::Ready => "Ready",
        State::InProgress => "In Progress",
        State::Review => "Review",
        State::Implemented => "Implemented",
    }
}

fn unblock_label(point: UnblockPoint) -> &'static str {
    match point {
        UnblockPoint::Implemented => "implemented",
        UnblockPoint::Review => "review",
        UnblockPoint::Merged => "merged",
    }
}

/// One level as text: each child of `group` in [`child_order`] as a box with its state badge, and
/// before each box one arrow line per incoming edge, such as `  v  after (review) from design`.
/// An edge naming no sibling is drawn with `?` and the missing slug. A task whose stacked-on
/// target was cancelled gets `retarget needed` in its badge.
pub fn level_graph(group: &Task, tree: &WorkTree) -> Vec<GraphLine> {
    let kids = child_order(group);
    let boxes: Vec<(String, Option<TaskId>)> = kids
        .iter()
        .map(|kid| {
            let mut badge = state_label(derive::state(kid, tree)).to_string();
            if derive::retarget_needed(kid, tree) {
                badge.push_str(", retarget needed");
            }
            (
                format!("{}  {}", kid.slug, badge),
                kid.id().map(str::to_string),
            )
        })
        .collect();
    let width = boxes
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    let rule = format!("+{}+", "-".repeat(width + 2));
    let mut lines = Vec::new();
    for (kid, (label, id)) in kids.iter().zip(&boxes) {
        for (kind, slug, point) in edges(kid) {
            let missing = !group.children.iter().any(|other| other.slug == slug);
            let text = if missing {
                format!(
                    "  ?  {} ({}) from {slug} (missing)",
                    kind.label(),
                    unblock_label(point)
                )
            } else {
                format!(
                    "  v  {} ({}) from {slug}",
                    kind.label(),
                    unblock_label(point)
                )
            };
            lines.push(GraphLine { text, node: None });
        }
        let padding = " ".repeat(width.saturating_sub(label.chars().count()));
        for text in [rule.clone(), format!("| {label}{padding} |"), rule.clone()] {
            lines.push(GraphLine {
                text,
                node: id.clone(),
            });
        }
    }
    lines
}
