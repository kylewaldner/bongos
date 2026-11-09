//! The multi-file operations: `task: split`, `task: move`, `task: move to project` and `task: make
//! one off`. Each returns a [`Change`] holding renames and files, which [`super::apply`] runs
//! through one `.op.toml` journal (D4): a crash leaves the old tree, the new tree, or a journal the
//! loader finishes. A task directory only ever moves by one rename, so it keeps its `id`, its files
//! and its history, and is never in two places.

use std::path::{Path, PathBuf};

use super::tasks::{check_goal, free_slug, give_up_todos};
use super::{Change, Context, Move, Outcome, Write, writable};
use crate::activity::to_utc;
use crate::layout::{self, CHATS_DIR, RUNS_DIR};
use crate::rules::{self, Place, Refusal};
use crate::slug::{oneoff_dir_name, oneoff_slug, slugify, unique_slug};
use crate::store::{Task, WorkTree};
use crate::types::TaskFile;

/// `task: split`: the leaf with `id` becomes a group whose one new child takes its work: the
/// todos, result, worktree, branch (with its base and rebase marks), the ship marker, the merge
/// override, `chats/` and `runs/`. The parent keeps its id, goal, history, edges and artifacts.
/// `goal` names the child; unset, the child takes the parent's goal.
///
/// This is how a leaf with a worktree, branch or result gains children ([`rules::check_add_child`]
/// refuses them otherwise).
pub fn split(tree: &WorkTree, ctx: &Context, id: &str, goal: Option<&str>) -> Outcome {
    let (place, mut parent) = writable(tree, id)?;
    rules::check_split(&place, tree)?;
    let goal = match goal {
        Some(goal) => check_goal(goal)?.to_string(),
        None => parent.goal.clone(),
    };
    // A leaf has no children, so only the reserved names are taken.
    let slug = free_slug(&slugify(&goal), &[]);
    let child_dir = layout::child_dir(&place.task.dir, &slug);

    let mut child = TaskFile::new(goal.as_str(), parent.kind);
    child.created = ctx.at;
    child.default_visibility = parent.default_visibility;
    let (moved, moves) = split_work(&mut parent, &mut child, place.task, &child_dir);

    child
        .history
        .push(ctx.entry("created", Some(format!("split from {}", place.slug()))));
    let detail = if moved.is_empty() {
        slug
    } else {
        format!("{slug}; moved: {}", moved.join(", "))
    };
    let entry = ctx.entry("split", Some(detail));
    parent.history.push(entry.clone());
    let dir = place.task.dir.clone();
    Ok(Change {
        dir: dir.clone(),
        writes: vec![
            Write::Task { dir, file: parent },
            Write::Task {
                dir: child_dir,
                file: child,
            },
        ],
        entry,
        moves,
    })
}

/// The work of `split`: moves the todos, result, planned result, worktree, branch (with its base
/// and rebase marks), ship marker and merge override from `parent` to `child`, and returns what
/// moved, by label, with the renames of `chats/` and `runs/` into `child_dir`.
pub fn split_work(
    parent: &mut TaskFile,
    child: &mut TaskFile,
    task: &Task,
    child_dir: &Path,
) -> (Vec<&'static str>, Vec<Move>) {
    let mut moved = Vec::new();
    if !parent.todos.is_empty() {
        moved.push("todos");
    }
    child.todos = std::mem::take(&mut parent.todos);
    take(&mut parent.result, &mut child.result, "result", &mut moved);
    take(
        &mut parent.planned_result,
        &mut child.planned_result,
        "planned result",
        &mut moved,
    );
    take(
        &mut parent.worktree,
        &mut child.worktree,
        "worktree",
        &mut moved,
    );
    take(&mut parent.branch, &mut child.branch, "branch", &mut moved);
    take(&mut parent.base, &mut child.base, "base", &mut moved);
    take(
        &mut parent.base_sha,
        &mut child.base_sha,
        "base sha",
        &mut moved,
    );
    take(
        &mut parent.rebase_ack_sha,
        &mut child.rebase_ack_sha,
        "rebase mark",
        &mut moved,
    );
    take(
        &mut parent.ship_overridden,
        &mut child.ship_overridden,
        "ship marker",
        &mut moved,
    );
    take(
        &mut parent.merge_is_ship,
        &mut child.merge_is_ship,
        "merge ship",
        &mut moved,
    );

    // Always renamed: a directory that is gone by then is skipped, and an empty one still moves.
    let mut moves = Vec::new();
    for (name, label) in [(CHATS_DIR, "chats/"), (RUNS_DIR, "runs/")] {
        if task.files.keys().any(|path| path.starts_with(name)) {
            moved.push(label);
        }
        moves.push(Move {
            from: task.dir.join(name),
            to: child_dir.join(name),
        });
    }
    (moved, moves)
}

/// Moves `from` into `to`, noting `name` in `moved` when there was something to move.
fn take<T>(from: &mut Option<T>, to: &mut Option<T>, name: &'static str, moved: &mut Vec<&str>) {
    if from.is_some() {
        moved.push(name);
    }
    *to = from.take();
}

/// `task: move`: the task with `id`, with its subtree and files, goes under the task with
/// `parent`. Its edges, and its old siblings' edges to it, are removed. A plain leaf parent
/// becomes a group and gives up its todos; a leaf with a worktree, branch or result is refused
/// (`task: split`).
pub fn move_task(tree: &WorkTree, ctx: &Context, id: &str, parent: &str) -> Outcome {
    let parent = Place::locate(tree, parent)?;
    relocate(tree, ctx, id, Destination::Under(parent), "moved")
}

/// `task: move to project`: the task with `id`, often a one-off, goes to the top level of the
/// project `project` (by slug), as by [`move_task`]. A one-off's directory drops its date.
pub fn move_to_project(tree: &WorkTree, ctx: &Context, id: &str, project: &str) -> Outcome {
    let found = tree
        .projects
        .iter()
        .find(|candidate| candidate.slug == project)
        .ok_or_else(|| format!("no project is named {project}"))?;
    let top = found
        .task
        .id()
        .ok_or_else(|| format!("{project} has no task.toml"))?;
    let parent = Place::locate(tree, top)?;
    relocate(
        tree,
        ctx,
        id,
        Destination::Under(parent),
        "moved to project",
    )
}

/// `task: make one off`: the task with `id`, with its subtree and files, leaves its parent for
/// `oneoffs/<activity>/<YYYY-MM-DD>-<slug>/`, dated by its creation, so a one-off moved to a
/// project and back returns to the same directory. Its edges are removed as by [`move_task`].
pub fn make_one_off(tree: &WorkTree, ctx: &Context, id: &str, activity: &str) -> Outcome {
    let activity = activity.trim();
    if activity.is_empty() {
        return Err("a one-off needs an activity".to_string());
    }
    relocate(tree, ctx, id, Destination::OneOff(activity), "made one-off")
}

/// Whether `task`, or a descendant that inherits its repository from it, has a worktree or a
/// branch.
fn holds_inherited_branch(task: &Task) -> bool {
    let Some(file) = &task.file else {
        return false;
    };
    file.worktree.is_some()
        || file.branch.is_some()
        || task.children.iter().any(|child| {
            child
                .file
                .as_ref()
                .is_some_and(|child_file| child_file.repository.is_none())
                && holds_inherited_branch(child)
        })
}

/// Where a moved task goes.
enum Destination<'a> {
    /// Under this task.
    Under(Place<'a>),
    /// A one-off filed under this activity.
    OneOff(&'a str),
}

/// The task files a move rewrites besides the moved task's own, each changed in turn.
#[derive(Default)]
struct Others {
    files: Vec<(PathBuf, TaskFile)>,
}

impl Others {
    /// The file of `task` with the changes so far, after its write guard.
    fn get(&self, task: &Task) -> Result<TaskFile, Refusal> {
        if let Some((_, file)) = self.files.iter().find(|(dir, _)| *dir == task.dir) {
            return Ok(file.clone());
        }
        task.check_writable()
            .map_err(|refused| refused.to_string())?;
        task.file
            .clone()
            .ok_or_else(|| format!("{} has no task.toml", task.slug))
    }

    fn put(&mut self, dir: &Path, file: TaskFile) {
        match self.files.iter_mut().find(|(written, _)| written == dir) {
            Some((_, existing)) => *existing = file,
            None => self.files.push((dir.to_path_buf(), file)),
        }
    }
}

fn relocate(
    tree: &WorkTree,
    ctx: &Context,
    id: &str,
    destination: Destination,
    event: &str,
) -> Outcome {
    let (place, mut file) = writable(tree, id)?;
    let slug = place.slug();
    if place.is_project() {
        return Err(format!("{slug} is a project; only its tasks can move"));
    }
    // The whole subtree moves, so no task in it may have an error badge.
    for descendant in place.task.walk().into_iter().skip(1) {
        descendant
            .check_writable()
            .map_err(|refused| refused.to_string())?;
    }
    let is_oneoff = place.project.is_none() && place.ancestors.is_empty();
    let base = if is_oneoff { oneoff_slug(slug) } else { slug };
    let mut others = Others::default();
    let mut details = Vec::new();

    let (new_dir, inherited) = match &destination {
        Destination::Under(parent) => {
            let parent_task = parent.task;
            if parent_task.dir.starts_with(&place.task.dir) {
                return Err(format!(
                    "{slug} cannot move under itself or one of its own tasks"
                ));
            }
            if place
                .ancestors
                .first()
                .is_some_and(|old| old.dir == parent_task.dir)
            {
                return Err(format!("{slug} is already under {}", parent.slug()));
            }
            rules::check_add_child(parent, tree)?;
            let mut parent_file = others.get(parent_task)?;
            let siblings: Vec<&str> = parent_task
                .children
                .iter()
                .map(|child| child.slug.as_str())
                .collect();
            let new_slug = free_slug(base, &siblings);
            let inherited = parent.repository(&parent_file);
            if give_up_todos(ctx, parent, tree, &mut parent_file, &new_slug) {
                others.put(&parent_task.dir, parent_file);
            }
            (layout::child_dir(&parent_task.dir, &new_slug), inherited)
        }
        Destination::OneOff(activity) => {
            if is_oneoff {
                return Err(format!("{slug} is already a one-off"));
            }
            let date = to_utc(&file.created)
                .ok_or_else(|| format!("{slug} has no creation date"))?
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
            let new_slug = unique_slug(base, &taken);
            let name = oneoff_dir_name(date, &new_slug);
            (layout::oneoff_dir(&tree.root, &activity_slug, &name), None)
        }
    };

    // Edges join siblings only, so a move removes the task's own and its old siblings' to it.
    let mut removed: Vec<String> = file.after.drain(..).map(|edge| edge.task).collect();
    removed.extend(file.stacked_on.take().map(|edge| edge.task));
    for sibling in place.siblings {
        let names_it = sibling.dir != place.task.dir
            && sibling.file.as_ref().is_some_and(|sibling_file| {
                sibling_file.after.iter().any(|edge| edge.task == slug)
                    || sibling_file
                        .stacked_on
                        .as_ref()
                        .is_some_and(|edge| edge.task == slug)
            });
        if !names_it {
            continue;
        }
        let mut edited = others.get(sibling)?;
        edited.after.retain(|edge| edge.task != slug);
        if edited
            .stacked_on
            .as_ref()
            .is_some_and(|edge| edge.task == slug)
        {
            edited.stacked_on = None;
        }
        edited
            .history
            .push(ctx.entry("edge removed", Some(format!("{slug} moved away"))));
        others.put(&sibling.dir, edited);
        removed.push(sibling.slug.clone());
    }
    if !removed.is_empty() {
        details.push(format!("edges removed: {}", removed.join(", ")));
    }

    // A worktree or branch stays in the repository it was made in, also one in the subtree that
    // inherits its repository from the moved task.
    let before = place.repository(&file);
    if file.repository.is_none()
        && holds_inherited_branch(place.task)
        && before.is_some()
        && before != inherited
    {
        file.repository = before;
        if let Some(repository) = &file.repository {
            details.push(format!("repository kept: {repository}"));
        }
    }
    match destination {
        Destination::Under(_) => file.activity = None,
        Destination::OneOff(activity) => file.activity = Some(activity.to_string()),
    }

    let shown = |dir: &Path| {
        dir.strip_prefix(&tree.root)
            .unwrap_or(dir)
            .display()
            .to_string()
    };
    let mut detail = format!("{} -> {}", shown(&place.task.dir), shown(&new_dir));
    for extra in details {
        detail.push_str("; ");
        detail.push_str(&extra);
    }
    let entry = ctx.entry(event, Some(detail));
    file.history.push(entry.clone());

    let mut writes = vec![Write::Task {
        dir: new_dir.clone(),
        file,
    }];
    writes.extend(
        others
            .files
            .into_iter()
            .map(|(dir, file)| Write::Task { dir, file }),
    );
    Ok(Change {
        dir: new_dir.clone(),
        writes,
        entry,
        moves: vec![Move {
            from: place.task.dir.clone(),
            to: new_dir,
        }],
    })
}
