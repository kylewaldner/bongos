//! Subtickets out: which tasks under a linked group have no ticket, which applied proposals were
//! already offered, which leaves were declined, and the pending-create write that makes a retry
//! after a crash create no second ticket.
//!
//! Detection uses only the `[[proposals]]` records of feature 26 (`state = "applied"`) in the
//! root's `task.toml` and the group's `linear_state`.

use bongos_work_model::commands::{Context, Outcome};
use bongos_work_model::derive::{self, Shape, State};
use bongos_work_model::graph::child_order;
use bongos_work_model::rules::{Place, Refusal};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{LinearOffered, LinearState, ProposalState, Status, TaskId};
use uuid::Uuid;

use crate::api::{IssueSummary, NewIssue};
use crate::sync::{
    edit, edit_linked, is_synced, link_of, linked_task, nearest_linked_ancestor, not_linked,
    task_path,
};

/// Which tasks `missing` lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The offer after an apply: leaves Kyle declined before are left out.
    AfterApply,
    /// `task: create missing subtickets`: every one, declined or not.
    Command,
}

/// A task without a ticket, in the order its ticket should be created: a parent before its
/// children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    pub task_id: TaskId,
    /// The task's goal.
    pub title: String,
    /// The leaf's `planned_result`, else empty.
    pub description: String,
    /// The task whose ticket the new ticket hangs under: a linked group, or an unlinked group
    /// earlier in the list.
    pub parent_task_id: TaskId,
    /// A group with no ticket yet: its leaves are its nested subtickets.
    pub is_group: bool,
    /// The idempotency key of a create already in flight; a retry reuses it.
    pub pending_id: Option<String>,
}

/// The open, unlinked tasks under the linked group `group_id` that need a ticket: leaves, and
/// unlinked groups with their leaves as nested subtickets, in the order of
/// `graph::child_order`. A group with nothing left to create under it is not listed. Read-only.
pub fn missing(tree: &WorkTree, group_id: &str, scope: Scope) -> Result<Vec<Missing>, Refusal> {
    let place = Place::locate(tree, group_id)?;
    let Some((_, state)) = link_of(place.task) else {
        return Err(not_linked(place.slug()));
    };
    let declined: &[String] = match scope {
        Scope::AfterApply => &state.declined,
        Scope::Command => &[],
    };
    let mut out = Vec::new();
    collect_missing(tree, place.task, declined, &mut out);
    Ok(out)
}

fn collect_missing(tree: &WorkTree, parent: &Task, declined: &[String], out: &mut Vec<Missing>) {
    let Some(parent_id) = parent.id() else {
        return;
    };
    for child in child_order(parent) {
        let Some(file) = &child.file else {
            continue;
        };
        if file.status != Status::Open
            || matches!(derive::state(child, tree), State::Done | State::Cancelled)
        {
            continue;
        }
        if is_synced(file) {
            // A linked subgroup: its own missing tasks hang under its ticket.
            collect_missing(tree, child, declined, out);
            continue;
        }
        if file.linear.is_some() {
            // A hand-set link already names a ticket.
            continue;
        }
        let pending_id = file
            .linear_state
            .as_ref()
            .and_then(|state| state.pending_create.clone());
        if derive::shape(child, tree) == Shape::Group {
            let mark = out.len();
            out.push(Missing {
                task_id: file.id.clone(),
                title: file.goal.clone(),
                description: String::new(),
                parent_task_id: parent_id.to_string(),
                is_group: true,
                pending_id,
            });
            collect_missing(tree, child, declined, out);
            if out.len() == mark + 1 {
                // Nothing under it needs a ticket.
                out.truncate(mark);
            }
        } else if !declined.contains(&file.id) {
            out.push(Missing {
                task_id: file.id.clone(),
                title: file.goal.clone(),
                description: file.planned_result.clone().unwrap_or_default(),
                parent_task_id: parent_id.to_string(),
                is_group: false,
                pending_id,
            });
        }
    }
}

/// The request that creates the ticket of `item`: its team is its parent's, and its parent is
/// the parent task's ticket, so the parent must have been created and linked first. `id` is the
/// create's idempotency key: `item.pending_id` when a create is in flight, else a new UUID.
pub fn create_input(tree: &WorkTree, item: &Missing, id: Uuid) -> Result<NewIssue, Refusal> {
    let parent = tree
        .task_by_id(&item.parent_task_id)
        .and_then(link_of)
        .map(|(_, state)| state)
        .ok_or_else(|| {
            format!(
                "the ticket of the parent of {} does not exist yet",
                item.title
            )
        })?;
    Ok(NewIssue {
        id,
        team_id: parent.team_id.clone(),
        parent_id: Some(parent.issue_id.clone()),
        title: item.title.clone(),
        description: item.description.clone(),
    })
}

/// The one write an unlinked task gets from this crate, and only because Kyle confirmed a
/// create: `pending_create = id` on a task with no link, under a linked group, before the request
/// is sent. A crash after the request leaves the key behind, so the retry reuses it and a
/// `Duplicate` answer links the ticket that exists instead of making a second.
///
/// Refused without `confirmed`, on a task that is already linked or already has a create in
/// flight (retry with the key it has), and when no linked group is above it.
pub fn begin_create(
    tree: &WorkTree,
    ctx: &Context,
    task_id: &str,
    id: Uuid,
    confirmed: bool,
) -> Outcome {
    edit(tree, ctx, task_id, |place, file| {
        let slug = place.slug();
        if !confirmed {
            return Err(format!("creating a ticket for {slug} needs a confirm"));
        }
        if let Some(identifier) = &file.linear {
            return Err(format!("{slug} already has the link {identifier}"));
        }
        if file
            .linear_state
            .as_ref()
            .is_some_and(|state| state.pending_create.is_some())
        {
            return Err(format!(
                "a create for {slug} is already in flight; retry it with its key"
            ));
        }
        if nearest_linked_ancestor(place).is_none() {
            return Err(format!("{slug} has no linked group above it"));
        }
        let state = file.linear_state.get_or_insert_with(LinearState::default);
        state.pending_create = Some(id.hyphenated().to_string());
        Ok(("ticket create started".to_string(), None))
    })
}

/// Completes a create: writes `linear` and the rest of `linear_state` from the ticket Linear
/// answered with (a created ticket, or one read by its UUID after a `Duplicate`) and clears
/// `pending_create`. `description` is what the ticket was created with. The goal stays: the
/// ticket was created with it. A group's `offered` list starts with the applied proposals under
/// it, as [`crate::sync::link`] does. Refused when another task already links the ticket.
pub fn finish_create(
    tree: &WorkTree,
    ctx: &Context,
    task_id: &str,
    issue: &IssueSummary,
    description: &str,
) -> Outcome {
    edit(tree, ctx, task_id, |place, file| {
        let slug = place.slug();
        let pending = file
            .linear_state
            .as_ref()
            .and_then(|state| state.pending_create.clone())
            .filter(|_| file.linear.is_none());
        let Some(pending) = pending else {
            return Err(format!("no create is in flight for {slug}"));
        };
        if !pending.eq_ignore_ascii_case(&issue.id) {
            return Err(format!(
                "{} is not the ticket being created for {slug}",
                issue.identifier
            ));
        }
        if let Some(other) = linked_task(tree, &issue.id, &issue.identifier, Some(&file.id)) {
            return Err(format!(
                "{} is linked to {}",
                issue.identifier,
                task_path(tree, other)
            ));
        }
        let mut state = LinearState {
            issue_id: issue.id.clone(),
            team_id: issue.team_id.clone(),
            url: issue.url.clone(),
            description: description.to_string(),
            ..LinearState::default()
        };
        if derive::shape(place.task, tree) == Shape::Group {
            state.offered = applied_under(place.task);
        }
        file.linear = Some(issue.identifier.clone());
        file.linear_state = Some(state);
        Ok((format!("created ticket {}", issue.identifier), None))
    })
}

/// An applied proposal under a linked group that was not offered tickets yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unoffered {
    /// The linked group the offer belongs to: the root itself when it is a linked group, else the
    /// nearest linked group above it.
    pub group_id: TaskId,
    pub root_id: TaskId,
    pub number: u32,
    pub hash: String,
    /// Who applied it: `kyle` opens the offer at once, `agent` or a workflow gets a chip.
    pub by: String,
}

/// Every applied `[[proposals]]` record whose root is a linked group, or has a linked group above
/// it, and whose `(root, number, hash)` is not in that group's `offered`.
pub fn unoffered(tree: &WorkTree) -> Vec<Unoffered> {
    let mut out = Vec::new();
    for project in &tree.projects {
        visit(tree, &project.task, None, &mut out);
    }
    for activity in &tree.activities {
        for oneoff in &activity.oneoffs {
            visit(tree, oneoff, None, &mut out);
        }
    }
    out
}

fn visit<'a>(tree: &WorkTree, task: &'a Task, nearest: Option<&'a Task>, out: &mut Vec<Unoffered>) {
    let group = if link_of(task).is_some() && derive::shape(task, tree) == Shape::Group {
        Some(task)
    } else {
        nearest
    };
    if let (Some(group), Some(file)) = (group, &task.file)
        && let Some((_, state)) = link_of(group)
        && let Some(group_id) = group.id()
    {
        for record in &file.proposals {
            let offered = state.offered.iter().any(|offered| {
                offered.root == file.id
                    && offered.number == record.number
                    && offered.hash == record.hash
            });
            if record.state == ProposalState::Applied && !offered {
                out.push(Unoffered {
                    group_id: group_id.to_string(),
                    root_id: file.id.clone(),
                    number: record.number,
                    hash: record.hash.clone(),
                    by: record.by.clone(),
                });
            }
        }
    }
    for child in &task.children {
        visit(tree, child, group, out);
    }
}

/// The applied records `group` would start with: those of the group itself and of the tasks
/// under it that no other linked group owns. Link and import seed `offered` with them, so only
/// later applies are offered tickets.
pub fn applied_under(group: &Task) -> Vec<LinearOffered> {
    let mut out = Vec::new();
    seed(group, true, &mut out);
    out
}

fn seed(task: &Task, top: bool, out: &mut Vec<LinearOffered>) {
    if !top && task.file.as_ref().is_some_and(is_synced) && !task.children.is_empty() {
        // A linked subgroup keeps its own `offered`.
        return;
    }
    if let Some(file) = &task.file {
        for record in &file.proposals {
            if record.state == ProposalState::Applied {
                out.push(LinearOffered {
                    root: file.id.clone(),
                    number: record.number,
                    hash: record.hash.clone(),
                    extra: Default::default(),
                });
            }
        }
    }
    for child in &task.children {
        seed(child, false, out);
    }
}

/// Records that `offers` were offered under the linked group `group_id`, whatever the answer, and
/// that `declined` leaves were not wanted ("Not now" or unchecked): they are not offered again
/// until `task: create missing subtickets`.
pub fn record_offered(
    tree: &WorkTree,
    ctx: &Context,
    group_id: &str,
    offers: &[Unoffered],
    declined: &[TaskId],
) -> Outcome {
    edit_linked(tree, ctx, group_id, |_, _, state, _| {
        for offer in offers.iter().filter(|offer| offer.group_id == group_id) {
            let known = state.offered.iter().any(|offered| {
                offered.root == offer.root_id
                    && offered.number == offer.number
                    && offered.hash == offer.hash
            });
            if !known {
                state.offered.push(LinearOffered {
                    root: offer.root_id.clone(),
                    number: offer.number,
                    hash: offer.hash.clone(),
                    extra: Default::default(),
                });
            }
        }
        for id in declined {
            if !state.declined.contains(id) {
                state.declined.push(id.clone());
            }
        }
        let detail = (!declined.is_empty()).then(|| format!("{} declined", declined.len()));
        Ok(("subtickets offered".to_string(), detail))
    })
}
