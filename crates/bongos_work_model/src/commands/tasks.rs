//! Creating tasks, editing them, and closing, dropping and reopening them.

use super::{Change, Context, Outcome, Write, edit_task, kind_name, single, writable};
use crate::activity::to_utc;
use crate::derive::{self, Shape, State};
use crate::layout;
use crate::rules::{self, Place, Refusal};
use crate::slug::{oneoff_dir_name, slugify, unique_slug};
use crate::store::WorkTree;
use crate::types::{Extra, Kind, Status, TaskFile, TaskId, Todo, TodoRole, TodoState};

/// What `task: new` asks for. Only the goal is required.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewTask {
    /// One sentence; it doubles as the title and gives the directory its slug.
    pub goal: String,
    /// `pr-ship` when unset (`spike` for a one-off).
    pub kind: Option<Kind>,
    /// Unset inherits from the parent, then the project.
    pub repository: Option<String>,
    /// The todo list the task starts with; unset gives the kind's [`default_todos`].
    pub todos: Option<Vec<Todo>>,
    /// The free-text work kind label; unset leaves the task without one.
    pub work_kind: Option<String>,
}

/// Where a new task goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Placement {
    /// Under the task with this id, which becomes (or stays) a group.
    Under(TaskId),
    /// A one-off with no parent, filed under this activity (short free text such as `support`).
    OneOff { activity: String },
}

/// What `task: edit` changes; `None` leaves a field as it is. Slugs and paths never change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskEdit {
    pub goal: Option<String>,
    pub kind: Option<Kind>,
    /// `Some(None)` clears the repository, so the task inherits it again.
    pub repository: Option<Option<String>>,
    /// The work kind label; `Some(None)` clears it. A blank label clears it too.
    pub work_kind: Option<Option<String>>,
}

/// The todo list a new leaf of `kind` starts with. The `pr-ship` and `pr-merge` lists set the
/// roles, so those tasks are stackable whatever their todos are later renamed to.
pub fn default_todos(kind: Kind) -> Vec<Todo> {
    let todo = |name: &str, role: Option<TodoRole>| Todo {
        id: None,
        name: name.to_string(),
        role,
        workflow: None,
        state: TodoState::Open,
        started: None,
        extra: Extra::new(),
    };
    match kind {
        Kind::PrShip => vec![
            todo("implement", Some(TodoRole::ProducesBranch)),
            todo("review", None),
            todo("ship", Some(TodoRole::Ships)),
        ],
        Kind::PrMerge => vec![
            todo("implement", Some(TodoRole::ProducesBranch)),
            todo("review", None),
            todo("merge", Some(TodoRole::Ships)),
        ],
        Kind::Document => vec![
            todo("draft", None),
            todo("review", None),
            todo("publish", None),
        ],
        Kind::Spike => vec![todo("explore", None), todo("write up", None)],
        Kind::Babysit => vec![todo("watch", None), todo("merge", None)],
        Kind::Other => vec![todo("do it", None)],
    }
}

pub(super) fn check_goal(goal: &str) -> Result<&str, Refusal> {
    let goal = goal.trim();
    if goal.is_empty() {
        Err("a task needs a goal".to_string())
    } else {
        Ok(goal)
    }
}

/// A work kind label trimmed; a blank one is none.
fn clean_work_kind(label: Option<String>) -> Option<String> {
    label
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
}

fn new_file(
    ctx: &Context,
    goal: &str,
    kind: Kind,
    repository: Option<String>,
    todos: Option<Vec<Todo>>,
    work_kind: Option<String>,
) -> TaskFile {
    let mut file = TaskFile::new(goal, kind);
    file.work_kind = clean_work_kind(work_kind);
    file.created = ctx.at;
    file.repository = repository;
    file.todos = todos.unwrap_or_else(|| default_todos(kind));
    file
}

/// `task: new`: a new leaf under a task or as a one-off.
pub fn new_task(tree: &WorkTree, ctx: &Context, placement: Placement, new: NewTask) -> Outcome {
    match placement {
        Placement::Under(parent) => new_child(tree, ctx, &parent, new),
        Placement::OneOff { activity } => new_oneoff(tree, ctx, &activity, new),
    }
}

/// `task: new child`: a new leaf under the task with `parent`. A leaf parent becomes a group and
/// gives up its todos, which the parent's history lists. A leaf with a worktree, branch or result
/// is refused (`task: split`).
pub fn new_child(tree: &WorkTree, ctx: &Context, parent: &str, new: NewTask) -> Outcome {
    let (place, mut parent_file) = writable(tree, parent)?;
    rules::check_add_child(&place, tree)?;
    let goal = check_goal(&new.goal)?;
    let siblings: Vec<&str> = place
        .task
        .children
        .iter()
        .map(|child| child.slug.as_str())
        .collect();
    let slug = free_slug(&slugify(goal), &siblings);
    let dir = layout::child_dir(&place.task.dir, &slug);

    let mut file = new_file(
        ctx,
        goal,
        new.kind.unwrap_or(Kind::PrShip),
        new.repository,
        new.todos,
        new.work_kind,
    );
    let entry = ctx.entry("created", Some(goal.to_string()));
    file.history.push(entry.clone());
    let mut writes = vec![Write::Task {
        dir: dir.clone(),
        file,
    }];

    if give_up_todos(ctx, &place, tree, &mut parent_file, &slug) {
        writes.push(Write::Task {
            dir: place.task.dir.clone(),
            file: parent_file,
        });
    }
    Ok(Change {
        dir,
        writes,
        entry,
        moves: Vec::new(),
    })
}

/// A slug for a new directory among `siblings`, never one of the reserved names (such as `runs`),
/// which the loader would not read as a task.
pub fn free_slug(base: &str, siblings: &[&str]) -> String {
    let mut taken: Vec<String> = siblings.iter().map(|slug| slug.to_string()).collect();
    loop {
        let slug = unique_slug(base, &taken);
        if !layout::is_reserved(&slug) {
            return slug;
        }
        taken.push(slug);
    }
}

/// When the plain leaf at `place` gains the child `child_slug` and so becomes a group, removes the
/// todos and the planned result from its `file` and records them in its history. Whether `file`
/// changed.
pub fn give_up_todos(
    ctx: &Context,
    place: &Place,
    tree: &WorkTree,
    file: &mut TaskFile,
    child_slug: &str,
) -> bool {
    if derive::shape(place.task, tree) == Shape::Group
        || (file.todos.is_empty() && file.planned_result.is_none())
    {
        return false;
    }
    let mut removed = Vec::new();
    if !file.todos.is_empty() {
        let names: Vec<&str> = file.todos.iter().map(|todo| todo.name.as_str()).collect();
        removed.push(format!("todos removed: {}", names.join(", ")));
        file.todos.clear();
    }
    if file.planned_result.take().is_some() {
        removed.push("planned result removed".to_string());
    }
    let detail = format!("{child_slug}; became a group, {}", removed.join(", "));
    file.history.push(ctx.entry("child added", Some(detail)));
    true
}

/// `task: new one off`: a new task with no parent, in
/// `oneoffs/<activity>/<YYYY-MM-DD>-<slug>/`.
pub fn new_oneoff(tree: &WorkTree, ctx: &Context, activity: &str, new: NewTask) -> Outcome {
    let activity = activity.trim();
    if activity.is_empty() {
        return Err("a one-off needs an activity".to_string());
    }
    let goal = check_goal(&new.goal)?;
    let date = to_utc(&ctx.at)
        .ok_or_else(|| "the time of the command has no date".to_string())?
        .date_naive();
    let activity_slug = slugify(activity);
    let prefix = oneoff_dir_name(date, "");
    let taken: Vec<&str> = tree
        .activities
        .iter()
        .filter(|existing| existing.slug == activity_slug)
        .flat_map(|existing| &existing.oneoffs)
        .filter_map(|oneoff| oneoff.slug.strip_prefix(&prefix))
        .collect();
    let slug = unique_slug(&slugify(goal), &taken);
    let dir = layout::oneoff_dir(&tree.root, &activity_slug, &oneoff_dir_name(date, &slug));

    let mut file = new_file(
        ctx,
        goal,
        new.kind.unwrap_or(Kind::Spike),
        new.repository,
        new.todos,
        new.work_kind,
    );
    file.activity = Some(activity.to_string());
    let entry = ctx.entry("created", Some(goal.to_string()));
    file.history.push(entry.clone());
    Ok(Change {
        dir: dir.clone(),
        writes: vec![Write::Task { dir, file }],
        entry,
        moves: Vec::new(),
    })
}

/// `task: new one off` while project repositories are on: a new task in the one-offs project
/// whose top group is `top`, a child of the top-level group of `activity` (the group whose
/// `activity` is that activity's slug), which is made with it when there is none yet.
pub fn new_oneoff_in_project(
    tree: &WorkTree,
    ctx: &Context,
    top: &str,
    activity: &str,
    new: NewTask,
) -> Outcome {
    let activity = activity.trim();
    if activity.is_empty() {
        return Err("a one-off needs an activity".to_string());
    }
    let goal = check_goal(&new.goal)?.to_string();
    let activity_slug = slugify(activity);
    if activity_slug.is_empty() {
        return Err(format!("{activity} cannot name an activity"));
    }
    let (place, mut top_file) = writable(tree, top)?;
    let kind = new.kind.unwrap_or(Kind::Spike);
    let existing = place.task.children.iter().find(|child| {
        child
            .file
            .as_ref()
            .and_then(|file| file.activity.as_deref())
            .is_some_and(|known| slugify(known) == activity_slug)
    });
    if let Some(group) = existing {
        let group_id = group
            .id()
            .ok_or_else(|| format!("{} has no task.toml", group.slug))?
            .to_string();
        let mut change = new_child(
            tree,
            ctx,
            &group_id,
            NewTask {
                goal,
                kind: Some(kind),
                ..new
            },
        )?;
        for write in &mut change.writes {
            if let Write::Task { dir, file } = write
                && *dir == change.dir
            {
                file.activity = Some(activity.to_string());
            }
        }
        return Ok(change);
    }

    rules::check_add_child(&place, tree)?;
    let siblings: Vec<&str> = place
        .task
        .children
        .iter()
        .map(|child| child.slug.as_str())
        .collect();
    let group_slug = free_slug(&activity_slug, &siblings);
    let group_dir = layout::child_dir(&place.task.dir, &group_slug);
    let mut group = TaskFile::new(activity_slug.clone(), Kind::Other);
    group.created = ctx.at;
    group.todos = Vec::new();
    group.activity = Some(activity_slug.clone());
    group
        .history
        .push(ctx.entry("created", Some(activity_slug)));

    let dir = layout::child_dir(&group_dir, &free_slug(&slugify(&goal), &[]));
    let mut file = new_file(ctx, &goal, kind, new.repository, new.todos, new.work_kind);
    file.activity = Some(activity.to_string());
    let entry = ctx.entry("created", Some(goal));
    file.history.push(entry.clone());
    let mut writes = vec![
        Write::Task {
            dir: group_dir,
            file: group,
        },
        Write::Task {
            dir: dir.clone(),
            file,
        },
    ];
    if give_up_todos(ctx, &place, tree, &mut top_file, &group_slug) {
        writes.push(Write::Task {
            dir: place.task.dir.clone(),
            file: top_file,
        });
    }
    Ok(Change {
        dir,
        writes,
        entry,
        moves: Vec::new(),
    })
}

/// `task: edit`: goal, kind and repository. A goal that comes from Linear
/// ([`rules::goal_locked`]) is refused; kind and repository stay editable. The directory keeps its slug, so artifacts, chats and
/// the worktree stay where they are.
pub fn edit(tree: &WorkTree, ctx: &Context, id: &str, edit: TaskEdit) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        let mut changed = Vec::new();
        if let Some(goal) = &edit.goal {
            let goal = check_goal(goal)?;
            // Setting a locked goal to what it already is changes nothing, so it is not refused.
            if goal != file.goal {
                if let Some(reason) = rules::goal_locked(file) {
                    return Err(reason);
                }
                changed.push(format!("goal: {:?} -> {goal:?}", file.goal));
                file.goal = goal.to_string();
            }
        }
        if let Some(kind) = edit.kind
            && kind != file.kind
        {
            changed.push(format!(
                "kind: {} -> {}",
                kind_name(file.kind),
                kind_name(kind)
            ));
            file.kind = kind;
        }
        if let Some(repository) = &edit.repository
            && *repository != file.repository
        {
            rules::check_repository_change(place, file, repository)?;
            let name = |repository: &Option<String>| {
                repository
                    .clone()
                    .unwrap_or_else(|| "inherited".to_string())
            };
            changed.push(format!(
                "repository: {} -> {}",
                name(&file.repository),
                name(repository)
            ));
            file.repository = repository.clone();
        }
        if let Some(label) = &edit.work_kind {
            let label = clean_work_kind(label.clone());
            if label != file.work_kind {
                let name = |label: &Option<String>| label.clone().unwrap_or_else(|| "none".to_string());
                changed.push(format!(
                    "work kind: {} -> {}",
                    name(&file.work_kind),
                    name(&label)
                ));
                file.work_kind = label;
            }
        }
        if changed.is_empty() {
            return Err(format!("{} already has those values", place.slug()));
        }
        Ok(("edited", Some(changed.join("; "))))
    })
}

/// `task: close`: done outright, whatever its todos.
pub fn close(tree: &WorkTree, ctx: &Context, id: &str) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        if file.status == Status::Done {
            return Err(format!("{} is already done", place.slug()));
        }
        file.status = Status::Done;
        file.closed = Some(ctx.at);
        Ok(("closed", None))
    })
}

/// `task: drop`: cancel the task. On a group with open descendants it needs `confirmed`, and
/// then cancels them too; the refusal without it lists them ([`rules::open_descendants`]).
pub fn drop_task(tree: &WorkTree, ctx: &Context, id: &str, confirmed: bool) -> Outcome {
    let (place, mut file) = writable(tree, id)?;
    if file.status == Status::Cancelled {
        return Err(format!("{} is already cancelled", place.slug()));
    }
    rules::check_drop(place.task, tree, confirmed)?;
    let open = rules::open_descendants(place.task, tree);
    let mut writes = Vec::new();
    for descendant in &open {
        descendant
            .check_writable()
            .map_err(|refused| refused.to_string())?;
        let Some(descendant_file) = &descendant.file else {
            continue;
        };
        let mut descendant_file = descendant_file.clone();
        descendant_file.status = Status::Cancelled;
        descendant_file.closed = Some(ctx.at);
        descendant_file
            .history
            .push(ctx.entry("dropped", Some(format!("with {}", place.slug()))));
        writes.push(Write::Task {
            dir: descendant.dir.clone(),
            file: descendant_file,
        });
    }
    file.status = Status::Cancelled;
    file.closed = Some(ctx.at);
    let detail = (!open.is_empty()).then(|| {
        let names: Vec<&str> = open.iter().map(|task| task.slug.as_str()).collect();
        format!("with {}", names.join(", "))
    });
    let mut change = single(ctx, &place, file, "dropped", detail);
    change.writes.extend(writes);
    Ok(change)
}

/// `task: reopen`: open again after close or drop. Children cancelled by a drop stay cancelled.
pub fn reopen(tree: &WorkTree, ctx: &Context, id: &str) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        if file.status == Status::Open {
            if derive::shape(place.task, tree) == Shape::Leaf
                && derive::state(place.task, tree) == State::Done
            {
                return Err(format!(
                    "{} is open but has no open todo; reopen one with todo: reopen",
                    place.slug()
                ));
            }
            return Err(format!("{} is already open", place.slug()));
        }
        file.status = Status::Open;
        file.closed = None;
        Ok(("reopened", None))
    })
}

/// What `task: start` made: the branch, what it was cut from, and the worktree holding it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StartedWorktree {
    pub branch: String,
    /// The branch it was cut from; `None` for a branch that already existed (a PR's head).
    pub base: Option<String>,
    pub base_sha: Option<String>,
    /// The absolute path of the worktree.
    pub worktree: String,
    /// The parent the worktree was meant for, when it was not writable and the worktree went to
    /// the central directory instead.
    pub fallback: Option<String>,
}

/// `task: start`: record the worktree just created for an open leaf and mark its current todo
/// started. Refused when the task already records a worktree, so a second one is never written
/// over the first.
pub fn record_worktree(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    started: StartedWorktree,
) -> Outcome {
    edit_task(tree, ctx, id, |place, file| {
        super::require_leaf(place, tree, "worktree")?;
        super::require_open(place.slug(), file)?;
        if let Some(existing) = &file.worktree {
            return Err(format!(
                "{} already has the worktree {existing}",
                place.slug()
            ));
        }
        file.branch = Some(started.branch.clone());
        file.base = started.base.clone();
        file.base_sha = started.base_sha.clone();
        file.worktree = Some(started.worktree.clone());
        if let Some(todo) = file
            .todos
            .iter_mut()
            .find(|todo| todo.state == TodoState::Open)
            && todo.started.is_none()
        {
            todo.started = Some(ctx.at);
        }
        Ok(match &started.fallback {
            Some(parent) => (
                "worktree created in the central directory",
                Some(format!(
                    "{} at {}; {parent} is not writable",
                    started.branch, started.worktree
                )),
            ),
            None => (
                "worktree created",
                Some(format!("{} at {}", started.branch, started.worktree)),
            ),
        })
    })
}
