//! Linking tasks to tickets, the refresh diff, suggestions and the commands that write them.
//!
//! Every command here is a pure transformation of the loaded tree that returns a [`Change`] for
//! `bongos_work_model::commands::apply`, or the refusal the UI shows. A command writes only the
//! task it names, and refuses a task that has no synced link (`linear` and `linear_state` both
//! set), with two exceptions: [`link`] itself, and the confirmed pending-create write of
//! [`crate::subtickets::begin_create`]. A task without a link never gains a field from this
//! crate.

use std::collections::BTreeMap;

use bongos_work_model::commands::{Change, Context, Outcome, Write, free_slug};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::{Place, Refusal};
use bongos_work_model::slug::slugify;
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{
    Extra, HistoryEntry, Kind, LinearState, TaskFile, TaskId, Todo, TodoRole, TodoState, layout,
};

use crate::api::{Issue, IssueSummary};
use crate::subtickets::applied_under;

/// The identifier of a synced link: `linear` and `linear_state` both set. A hand-set `linear`
/// alone and a pending create are not synced.
pub fn linked_identifier(file: &TaskFile) -> Option<&str> {
    file.linear_state.as_ref()?;
    file.linear.as_deref()
}

pub fn is_synced(file: &TaskFile) -> bool {
    linked_identifier(file).is_some()
}

/// The synced link of `task`: identifier and its state.
pub(crate) fn link_of(task: &Task) -> Option<(&str, &LinearState)> {
    let file = task.file.as_ref()?;
    Some((file.linear.as_deref()?, file.linear_state.as_ref()?))
}

/// The linked groups above `task`'s place, nearest first: the groups whose ticket the task's
/// ticket would hang under.
pub fn nearest_linked_ancestor<'a>(place: &Place<'a>) -> Option<&'a Task> {
    place
        .ancestors
        .iter()
        .copied()
        .find(|ancestor| link_of(ancestor).is_some())
}

/// Where a task is, for messages: its ancestors' slugs, then its own, joined by `/`.
pub fn task_path(tree: &WorkTree, task: &Task) -> String {
    let Some(id) = task.id() else {
        return task.slug.clone();
    };
    match Place::locate(tree, id) {
        Ok(place) => {
            let mut parts: Vec<&str> = place
                .ancestors
                .iter()
                .rev()
                .map(|ancestor| ancestor.slug.as_str())
                .collect();
            parts.push(&task.slug);
            parts.join("/")
        }
        Err(_) => task.slug.clone(),
    }
}

/// A task that already links the ticket, by Linear's UUID or by identifier (a hand-set link
/// counts), other than `except`.
pub fn linked_task<'a>(
    tree: &'a WorkTree,
    issue_id: &str,
    identifier: &str,
    except: Option<&str>,
) -> Option<&'a Task> {
    tree.tasks().into_iter().find(|task| {
        let Some(file) = &task.file else {
            return false;
        };
        if except.is_some_and(|id| id == file.id) {
            return false;
        }
        let same_identifier = file
            .linear
            .as_deref()
            .is_some_and(|linked| linked.eq_ignore_ascii_case(identifier));
        let same_id = file
            .linear_state
            .as_ref()
            .is_some_and(|state| !state.issue_id.is_empty() && state.issue_id == issue_id);
        same_identifier || same_id
    })
}

pub(crate) fn entry(ctx: &Context, event: &str, detail: Option<String>) -> HistoryEntry {
    HistoryEntry {
        at: ctx.at,
        by: ctx.by.clone(),
        event: event.to_string(),
        detail,
        extra: Extra::new(),
    }
}

/// Runs `change` on a copy of the file of the task `id`, after its write guard, and records the
/// event it names. The one place a command here becomes a [`Change`] of a single `task.toml`.
pub(crate) fn edit(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    change: impl FnOnce(&Place, &mut TaskFile) -> Result<(String, Option<String>), Refusal>,
) -> Outcome {
    let place = Place::locate(tree, id)?;
    place
        .task
        .check_writable()
        .map_err(|refused| refused.to_string())?;
    let mut file = place
        .task
        .file
        .clone()
        .ok_or_else(|| format!("{} has no task.toml", place.slug()))?;
    let (event, detail) = change(&place, &mut file)?;
    let entry = entry(ctx, &event, detail);
    file.history.push(entry.clone());
    let dir = place.task.dir.clone();
    Ok(Change {
        dir: dir.clone(),
        writes: vec![Write::Task { dir, file }],
        entry,
        moves: Vec::new(),
    })
}

/// [`edit`] for a task with a synced link: anything else is refused before `change` runs. The
/// closure gets the state to change, the identifier and the place.
pub(crate) fn edit_linked(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    change: impl FnOnce(
        &Place,
        &mut TaskFile,
        &mut LinearState,
        &str,
    ) -> Result<(String, Option<String>), Refusal>,
) -> Outcome {
    edit(tree, ctx, id, |place, file| {
        let Some(identifier) = linked_identifier(file).map(str::to_string) else {
            return Err(not_linked(place.slug()));
        };
        let mut state = file.linear_state.take().unwrap_or_default();
        let result = change(place, file, &mut state, &identifier);
        file.linear_state = Some(state);
        result
    })
}

pub(crate) fn not_linked(slug: &str) -> Refusal {
    format!("{slug} is not linked to Linear; run task: link to linear first")
}

/// `task: link to linear`: `task_id` takes the ticket `issue` and its title.
///
/// Refused when the task already has a synced link (unlink first) or a create in flight, when
/// another task already links the ticket, and when a group would link a ticket that is itself a
/// subticket of its linked parent group's ticket (it would duplicate the level). A leaf may link
/// a ticket with subtickets; they are ignored. A group's `offered` list starts with every applied
/// proposal under it, so only later applies are offered tickets.
pub fn link(tree: &WorkTree, ctx: &Context, task_id: &str, issue: &Issue) -> Outcome {
    edit(tree, ctx, task_id, |place, file| {
        let slug = place.slug();
        if let Some(existing) = linked_identifier(file) {
            return Err(format!(
                "{slug} is already linked to {existing}; unlink it first"
            ));
        }
        if file
            .linear_state
            .as_ref()
            .is_some_and(|state| state.pending_create.is_some())
        {
            return Err(format!("a ticket is being created for {slug}"));
        }
        if let Some(other) = linked_task(tree, &issue.id, &issue.identifier, Some(&file.id)) {
            return Err(format!(
                "{} is linked to {}",
                issue.identifier,
                task_path(tree, other)
            ));
        }
        let group = derive::shape(place.task, tree) == Shape::Group;
        if group
            && let Some(parent) = nearest_linked_ancestor(place)
            && let Some((_, parent_state)) = link_of(parent)
            && issue
                .parent
                .as_ref()
                .is_some_and(|ticket_parent| ticket_parent.id == parent_state.issue_id)
        {
            return Err(format!(
                "{} is a subticket of {}, which {} is already linked under; link its leaves instead",
                issue.identifier,
                link_of(parent).map_or("", |(identifier, _)| identifier),
                parent.slug,
            ));
        }
        let mut state = LinearState {
            issue_id: issue.id.clone(),
            team_id: issue.team_id.clone(),
            url: issue.url.clone(),
            description: issue.description.clone(),
            ..LinearState::default()
        };
        if group {
            state.offered = applied_under(place.task);
        }
        if !issue.title.trim().is_empty() {
            file.goal = issue.title.trim().to_string();
        }
        file.linear = Some(issue.identifier.clone());
        file.linear_state = Some(state);
        Ok((format!("linked to {}", issue.identifier), None))
    })
}

/// `task: unlink from linear`: removes `linear` and `linear_state`, keeps the goal. No request.
pub fn unlink(tree: &WorkTree, ctx: &Context, task_id: &str) -> Outcome {
    edit(tree, ctx, task_id, |place, file| {
        let Some(identifier) = linked_identifier(file).map(str::to_string) else {
            return Err(not_linked(place.slug()));
        };
        file.linear = None;
        file.linear_state = None;
        Ok((format!("unlinked from {identifier}"), None))
    })
}

/// One linked task whose ticket changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskChange {
    pub task_id: TaskId,
    pub issue_id: String,
    pub identifier: String,
    /// The new title (the goal), with the old one.
    pub title: Option<Renamed>,
    /// The new description.
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renamed {
    pub old: String,
    pub new: String,
}

/// What a refresh found, per linked task. Pure: the app decides what to write and show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RefreshDiff {
    /// Linked tasks with a new title or description.
    pub changes: Vec<TaskChange>,
    /// Linked tasks whose ticket was not returned: moved or deleted. Shown "linear: not found",
    /// never unlinked automatically.
    pub not_found: Vec<TaskId>,
    /// Per linked group, the tickets under its ticket that no task links and nobody dismissed.
    pub suggestions: BTreeMap<TaskId, Vec<IssueSummary>>,
}

impl RefreshDiff {
    /// Nothing to write and nothing to show.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.not_found.is_empty() && self.suggestions.is_empty()
    }
}

/// The refresh diff of the synced tasks named by `scope` (a refresh reads only some of them)
/// against `issues`, the tickets Linear returned (unknown ones are left out). A ticket is matched
/// by its UUID, then by identifier. A hand-set link and a pending create are not looked at.
pub fn refresh_diff(tree: &WorkTree, scope: &[TaskId], issues: &[Issue]) -> RefreshDiff {
    let mut diff = RefreshDiff::default();
    for task in tree.tasks() {
        let (Some(file), Some((identifier, state))) = (&task.file, link_of(task)) else {
            continue;
        };
        if !scope.contains(&file.id) {
            continue;
        }
        let Some(issue) = issues.iter().find(|issue| {
            (!state.issue_id.is_empty() && issue.id == state.issue_id)
                || issue.identifier.eq_ignore_ascii_case(identifier)
        }) else {
            diff.not_found.push(file.id.clone());
            continue;
        };
        let title =
            (!issue.title.trim().is_empty() && issue.title.trim() != file.goal).then(|| Renamed {
                old: file.goal.clone(),
                new: issue.title.trim().to_string(),
            });
        let description =
            (issue.description != state.description).then(|| issue.description.clone());
        if title.is_some() || description.is_some() {
            diff.changes.push(TaskChange {
                task_id: file.id.clone(),
                issue_id: issue.id.clone(),
                identifier: identifier.to_string(),
                title,
                description,
            });
        }
        if derive::shape(task, tree) == Shape::Group {
            let found = suggestions(tree, &file.id, issue);
            if !found.is_empty() {
                diff.suggestions.insert(file.id.clone(), found);
            }
        }
    }
    diff
}

/// The one write of a refresh: every task of `diff.changes` that still has the link the diff was
/// computed from gets its new title and description, with history `title from Linear: "old" ->
/// "new"` (by whoever `ctx` names, `linear`). A task that was unlinked, relinked, or already
/// changed meanwhile is skipped. `None` when nothing is left to write: a refresh that finds
/// nothing new writes nothing.
pub fn refresh_change(tree: &WorkTree, ctx: &Context, diff: &RefreshDiff) -> Option<Change> {
    let mut writes = Vec::new();
    let mut first_entry = None;
    let mut first_dir = None;
    for change in &diff.changes {
        let Ok(place) = Place::locate(tree, &change.task_id) else {
            continue;
        };
        if place.task.check_writable().is_err() {
            continue;
        }
        let Some(mut file) = place.task.file.clone() else {
            continue;
        };
        let same_link =
            link_of(place.task).is_some_and(|(_, state)| state.issue_id == change.issue_id);
        if !same_link {
            continue;
        }
        let mut written = false;
        if let Some(title) = &change.title
            && file.goal == title.old
        {
            file.goal = title.new.clone();
            let entry = entry(
                ctx,
                "title from Linear",
                Some(format!("\"{}\" -> \"{}\"", title.old, title.new)),
            );
            first_entry.get_or_insert_with(|| entry.clone());
            file.history.push(entry);
            written = true;
        }
        if let Some(description) = &change.description
            && let Some(state) = file.linear_state.as_mut()
            && state.description != *description
        {
            state.description = description.clone();
            let entry = entry(ctx, "description from Linear", None);
            first_entry.get_or_insert_with(|| entry.clone());
            file.history.push(entry);
            written = true;
        }
        if written {
            first_dir.get_or_insert_with(|| place.task.dir.clone());
            writes.push(Write::Task {
                dir: place.task.dir.clone(),
                file,
            });
        }
    }
    Some(Change {
        dir: first_dir?,
        writes,
        entry: first_entry?,
        moves: Vec::new(),
    })
}

/// The children of the ticket of the linked group `group_id` that no task links (by UUID or
/// identifier, a pending create's UUID included) and nobody dismissed. Empty for a task that is
/// not a linked group.
pub fn suggestions(tree: &WorkTree, group_id: &str, issue: &Issue) -> Vec<IssueSummary> {
    let Some(group) = tree.task_by_id(group_id) else {
        return Vec::new();
    };
    let Some((_, state)) = link_of(group) else {
        return Vec::new();
    };
    if derive::shape(group, tree) != Shape::Group {
        return Vec::new();
    }
    issue
        .children
        .iter()
        .filter(|child| {
            !state
                .dismissed
                .iter()
                .any(|dismissed| dismissed.eq_ignore_ascii_case(&child.identifier))
                && linked_task(tree, &child.id, &child.identifier, None).is_none()
                && !pending_create_of(tree, &child.id)
        })
        .map(Issue::summary)
        .collect()
}

/// Whether a task has a create in flight with this UUID: its ticket may already exist in Linear.
fn pending_create_of(tree: &WorkTree, issue_id: &str) -> bool {
    tree.tasks().into_iter().any(|task| {
        task.file
            .as_ref()
            .and_then(|file| file.linear_state.as_ref())
            .and_then(|state| state.pending_create.as_deref())
            .is_some_and(|pending| pending.eq_ignore_ascii_case(issue_id))
    })
}

/// An unlinked leaf under `group_id` whose goal equals the ticket's title: the suggestion is
/// offered as "link to <leaf>" instead of a new leaf.
pub fn matching_leaf<'a>(
    tree: &'a WorkTree,
    group_id: &str,
    suggestion: &IssueSummary,
) -> Option<&'a Task> {
    let group = tree.task_by_id(group_id)?;
    group.walk().into_iter().skip(1).find(|task| {
        let Some(file) = &task.file else {
            return false;
        };
        derive::shape(task, tree) == Shape::Leaf
            && file.linear.is_none()
            && file
                .linear_state
                .as_ref()
                .is_none_or(|state| state.pending_create.is_none())
            && file
                .goal
                .trim()
                .eq_ignore_ascii_case(suggestion.title.trim())
    })
}

/// Accepts a suggestion: a new linked leaf under the linked group `group_id` with one
/// `{ name = "plan", role = "plans" }` todo and the ticket's title and description, in one
/// write. `kind` is the placement's default kind. To link an existing leaf instead, use
/// [`link`] on it.
pub fn accept(
    tree: &WorkTree,
    ctx: &Context,
    group_id: &str,
    issue: &Issue,
    kind: Kind,
) -> Outcome {
    let place = Place::locate(tree, group_id)?;
    place
        .task
        .check_writable()
        .map_err(|refused| refused.to_string())?;
    let Some((group_identifier, _)) = link_of(place.task) else {
        return Err(not_linked(place.slug()));
    };
    if derive::shape(place.task, tree) != Shape::Group {
        return Err(format!(
            "{} is a leaf; only a group has suggestions",
            place.slug()
        ));
    }
    if let Some(other) = linked_task(tree, &issue.id, &issue.identifier, None) {
        return Err(format!(
            "{} is linked to {}",
            issue.identifier,
            task_path(tree, other)
        ));
    }
    let title = issue.title.trim();
    if title.is_empty() {
        return Err(format!("{} has no title", issue.identifier));
    }
    let siblings: Vec<&str> = place
        .task
        .children
        .iter()
        .map(|child| child.slug.as_str())
        .collect();
    let slug = free_slug(&slugify(title), &siblings);
    let dir = layout::child_dir(&place.task.dir, &slug);

    let mut file = TaskFile::new(title, kind);
    file.created = ctx.at;
    file.todos = vec![plan_todo()];
    file.linear = Some(issue.identifier.clone());
    file.linear_state = Some(LinearState {
        issue_id: issue.id.clone(),
        team_id: issue.team_id.clone(),
        url: issue.url.clone(),
        description: issue.description.clone(),
        ..LinearState::default()
    });
    file.history.push(entry(
        ctx,
        "created",
        Some(format!(
            "{} from a suggestion under {group_identifier}",
            issue.identifier
        )),
    ));
    let linked = entry(ctx, &format!("linked to {}", issue.identifier), None);
    file.history.push(linked.clone());
    Ok(Change {
        dir: dir.clone(),
        writes: vec![Write::Task { dir, file }],
        entry: linked,
        moves: Vec::new(),
    })
}

/// The one todo of a leaf made from a ticket (26 D9).
pub fn plan_todo() -> Todo {
    Todo {
        name: "plan".to_string(),
        role: Some(TodoRole::Plans),
        workflow: None,
        state: TodoState::Open,
        started: None,
        extra: Extra::new(),
    }
}

/// Dismisses a suggestion for good: its identifier joins the group's `dismissed` list.
pub fn dismiss(tree: &WorkTree, ctx: &Context, group_id: &str, identifier: &str) -> Outcome {
    let identifier = identifier.trim().to_ascii_uppercase();
    edit_linked(tree, ctx, group_id, |place, _, state, _| {
        if derive::shape(place.task, tree) != Shape::Group {
            return Err(format!(
                "{} is a leaf; only a group has suggestions",
                place.slug()
            ));
        }
        if state
            .dismissed
            .iter()
            .any(|dismissed| dismissed.eq_ignore_ascii_case(&identifier))
        {
            return Err(format!("{identifier} is already dismissed"));
        }
        state.dismissed.push(identifier.clone());
        Ok((format!("dismissed {identifier}"), None))
    })
}
