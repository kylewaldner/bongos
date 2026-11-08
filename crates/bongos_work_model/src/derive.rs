//! Derived state as pure functions over the loaded tree: a task's shape, whether it is stackable,
//! its state and whether it is blocked. Nothing here reads or writes a file, and no derived value
//! is ever stored; every snapshot recomputes them, so reaching an unblock point clears a badge
//! without a reload.
//!
//! Todos are matched by `role`, never by `name`, so renaming the implement todo changes nothing.
//! The design's `state(task, tree, settings)` takes no settings here: nothing in the state depends
//! on them yet.

use std::path::Path;

use crate::store::{Task, WorkTree};
use crate::types::{
    Edge, Kind, ResultKind, ResultState, Status, TaskFile, Todo, TodoRole, TodoState, UnblockPoint,
};

/// Whether a task holds other tasks or does the work itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shape {
    Group,
    Leaf,
}

/// The derived state of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    Cancelled,
    Done,
    /// The PR is merged but the ships todo is still open.
    Merged,
    Blocked,
    Ready,
    InProgress,
    Review,
    Implemented,
}

/// A group when it has children, else a leaf. A project's top task is always a group, even while
/// it is empty.
pub fn shape(task: &Task, tree: &WorkTree) -> Shape {
    if !task.children.is_empty()
        || tree
            .projects
            .iter()
            .any(|project| project.task.dir == task.dir)
    {
        Shape::Group
    } else {
        Shape::Leaf
    }
}

/// Whether the task has a `produces-branch` todo and a `ships` todo, whatever they are named.
pub fn stackable(task: &Task) -> bool {
    task.file.as_ref().is_some_and(|file| {
        has_role(&file.todos, TodoRole::ProducesBranch) && has_role(&file.todos, TodoRole::Ships)
    })
}

/// The first todo still open.
pub fn current_todo(task: &Task) -> Option<&Todo> {
    task.file
        .as_ref()?
        .todos
        .iter()
        .find(|todo| todo.state == TodoState::Open)
}

/// A leaf whose todos are all closed. Cancelled and skipped todos never block. A leaf with no
/// todos at all is not done: it has not been set up yet, and calling it done would also mark the
/// group around it done.
pub fn leaf_done(task: &Task) -> bool {
    task.file.as_ref().is_some_and(leaf_done_file)
}

/// The derived state of `task` within `tree`.
///
/// Leaves of the PR kinds (`pr-ship`, `pr-merge`) take the first match of: Cancelled, Done,
/// Merged, Blocked, Ready, In Progress, Review, Implemented. Other leaves take Cancelled, Done,
/// Blocked, Ready, In Progress. A leaf's Blocked includes blocking inherited from its parent
/// groups. A group takes Cancelled, Done, Blocked (by its own edges only), In Progress, Ready.
/// A task whose `task.toml` has never parsed is Ready.
pub fn state(task: &Task, tree: &WorkTree) -> State {
    let Some(file) = &task.file else {
        return State::Ready;
    };
    if shape(task, tree) == Shape::Group {
        return group_state(task, file, tree);
    }
    let stage = if is_pr_kind(file.kind) {
        pr_stage(file)
    } else {
        other_stage(file)
    };
    match stage {
        State::Ready | State::InProgress | State::Review | State::Implemented => {
            if blocked(task, tree) {
                State::Blocked
            } else {
                stage
            }
        }
        State::Cancelled | State::Done | State::Merged | State::Blocked => stage,
    }
}

/// Whether the task is blocked. True when any `after` edge has not reached its unblock point,
/// when the `stacked_on` target has not reached its point or is Cancelled ("retarget needed", see
/// [`retarget_needed`]), or when its parent group is blocked: blocking inherits downward.
///
/// A target reaches a point as follows (the points order Implemented < Review < Merged). A PR
/// target reaches the points its own state has passed, and every point once Done or once its PR
/// is merged. A non-PR target reaches every point when Done. A group target reaches a point when
/// every non-cancelled leaf under it has. A Cancelled `after` target counts as reached, because
/// cancelled items never block.
///
/// A broken edge, one naming a sibling that does not exist, does not block. It would otherwise
/// block until someone edited the file, and the sibling may appear a moment later (an agent
/// creating it). The task is flagged through [`Task::broken_edges`] instead, which the loader
/// fills. A sibling whose `task.toml` has never parsed has reached nothing, so the edge on it
/// blocks until that file is fixed.
pub fn blocked(task: &Task, tree: &WorkTree) -> bool {
    let Some(context) = context(tree, task) else {
        return false;
    };
    own_edges_block(task, context.siblings)
        || context.parent.is_some_and(|parent| blocked(parent, tree))
}

/// Whether the task's `stacked_on` target is Cancelled and so needs a new target.
pub fn retarget_needed(task: &Task, tree: &WorkTree) -> bool {
    let Some(file) = &task.file else {
        return false;
    };
    let Some(stacked_on) = &file.stacked_on else {
        return false;
    };
    context(tree, task)
        .and_then(|context| sibling(context.siblings, &stacked_on.task))
        .is_some_and(is_cancelled)
}

fn has_role(todos: &[Todo], role: TodoRole) -> bool {
    todos.iter().any(|todo| todo.role == Some(role))
}

fn is_pr_kind(kind: Kind) -> bool {
    matches!(kind, Kind::PrShip | Kind::PrMerge)
}

fn leaf_done_file(file: &TaskFile) -> bool {
    !file.todos.is_empty() && file.todos.iter().all(|todo| todo.state != TodoState::Open)
}

/// A todo that has begun: it has a start time, or it was closed as done (an agent editing the
/// file may complete a todo without writing `started`).
fn todo_started(todo: &Todo) -> bool {
    todo.started.is_some() || todo.state == TodoState::Done
}

fn leaf_started(file: &TaskFile) -> bool {
    file.worktree.is_some() || file.todos.iter().any(todo_started)
}

fn result_merged(file: &TaskFile) -> bool {
    file.result
        .as_ref()
        .is_some_and(|result| result.state == Some(ResultState::Merged))
}

/// The state of a PR-kind leaf before Blocked is considered: the Blocked step sits between Merged
/// and Ready in the order, so [`state`] applies it to the four states after it.
fn pr_stage(file: &TaskFile) -> State {
    if file.status == Status::Cancelled {
        return State::Cancelled;
    }
    let merged = result_merged(file);
    if file.status == Status::Done || leaf_done_file(file) || (file.kind == Kind::PrMerge && merged)
    {
        return State::Done;
    }
    let ships_open = file
        .todos
        .iter()
        .any(|todo| todo.role == Some(TodoRole::Ships) && todo.state == TodoState::Open);
    if file.kind == Kind::PrShip && merged && ships_open {
        return State::Merged;
    }
    if !leaf_started(file) {
        return State::Ready;
    }
    let produces = file
        .todos
        .iter()
        .position(|todo| todo.role == Some(TodoRole::ProducesBranch));
    if produces.is_some_and(|index| {
        file.todos
            .get(index)
            .is_some_and(|todo| todo.state == TodoState::Open)
    }) {
        return State::InProgress;
    }
    let later_started = produces.is_some_and(|index| {
        file.todos
            .iter()
            .skip(index + 1)
            .any(|todo| todo_started(todo))
    });
    let pr_open = file.result.as_ref().is_some_and(|result| {
        matches!(result.kind, ResultKind::Pr | ResultKind::Prs)
            && matches!(
                result.state,
                Some(ResultState::Open | ResultState::Approved)
            )
    });
    if later_started || pr_open {
        return State::Review;
    }
    if produces.is_some() {
        State::Implemented
    } else {
        // Without a produces-branch todo there is no branch to be done with.
        State::InProgress
    }
}

fn other_stage(file: &TaskFile) -> State {
    if file.status == Status::Cancelled {
        State::Cancelled
    } else if file.status == Status::Done || leaf_done_file(file) {
        State::Done
    } else if leaf_started(file) {
        State::InProgress
    } else {
        State::Ready
    }
}

fn group_state(task: &Task, file: &TaskFile, tree: &WorkTree) -> State {
    if is_cancelled(task) {
        return State::Cancelled;
    }
    if file.status == Status::Done {
        return State::Done;
    }
    let mut live = task.children.iter().filter(|child| !is_cancelled(child));
    if live.clone().next().is_some() && live.all(|child| state(child, tree) == State::Done) {
        return State::Done;
    }
    if let Some(context) = context(tree, task)
        && own_edges_block(task, context.siblings)
    {
        return State::Blocked;
    }
    if task
        .children
        .iter()
        .any(|child| !is_cancelled(child) && has_started(child))
    {
        State::InProgress
    } else {
        State::Ready
    }
}

/// Whether work has begun on the task or under it, ignoring Blocked.
fn has_started(task: &Task) -> bool {
    if !task.children.is_empty() {
        return task
            .children
            .iter()
            .any(|child| !is_cancelled(child) && has_started(child));
    }
    task.file
        .as_ref()
        .is_some_and(|file| file.status == Status::Done || leaf_started(file))
}

/// A task is cancelled when its `status` says so, or when it has children and all are cancelled.
fn is_cancelled(task: &Task) -> bool {
    if task
        .file
        .as_ref()
        .is_some_and(|file| file.status == Status::Cancelled)
    {
        return true;
    }
    !task.children.is_empty() && task.children.iter().all(is_cancelled)
}

fn rank(point: UnblockPoint) -> u8 {
    match point {
        UnblockPoint::Implemented => 1,
        UnblockPoint::Review => 2,
        UnblockPoint::Merged => 3,
    }
}

/// Whether `target` (not cancelled) has reached `point`.
fn reached(target: &Task, point: UnblockPoint) -> bool {
    if is_cancelled(target) {
        return true;
    }
    if !target.children.is_empty() {
        return target.children.iter().all(|child| reached(child, point));
    }
    let Some(file) = &target.file else {
        return false;
    };
    let stage = if is_pr_kind(file.kind) {
        pr_stage(file)
    } else {
        other_stage(file)
    };
    let stage_rank = match stage {
        State::Done => return true,
        State::Merged => 3,
        State::Review => 2,
        State::Implemented => 1,
        _ => 0,
    };
    let merged_rank = if is_pr_kind(file.kind) && result_merged(file) {
        3
    } else {
        0
    };
    stage_rank.max(merged_rank) >= rank(point)
}

fn own_edges_block(task: &Task, siblings: &[Task]) -> bool {
    let Some(file) = &task.file else {
        return false;
    };
    let after_blocks = file.after.iter().any(|edge: &Edge| {
        sibling(siblings, &edge.task)
            .is_some_and(|target| target.dir != task.dir && !reached(target, edge.unblock))
    });
    let stacked_blocks = file.stacked_on.as_ref().is_some_and(|edge| {
        sibling(siblings, &edge.task).is_some_and(|target| {
            target.dir != task.dir && (is_cancelled(target) || !reached(target, edge.unblock))
        })
    });
    after_blocks || stacked_blocks
}

fn sibling<'a>(siblings: &'a [Task], slug: &str) -> Option<&'a Task> {
    siblings.iter().find(|task| task.slug == slug)
}

/// Where a task sits: its parent group and the tasks sharing its parent directory.
struct Context<'a> {
    parent: Option<&'a Task>,
    siblings: &'a [Task],
}

fn context<'a>(tree: &'a WorkTree, task: &Task) -> Option<Context<'a>> {
    for project in &tree.projects {
        if let Some(found) = find_context(std::slice::from_ref(&project.task), None, &task.dir) {
            return Some(found);
        }
    }
    for activity in &tree.activities {
        if let Some(found) = find_context(&activity.oneoffs, None, &task.dir) {
            return Some(found);
        }
    }
    None
}

fn find_context<'a>(
    siblings: &'a [Task],
    parent: Option<&'a Task>,
    dir: &Path,
) -> Option<Context<'a>> {
    for task in siblings {
        if task.dir == dir {
            return Some(Context { parent, siblings });
        }
        if let Some(found) = find_context(&task.children, Some(task), dir) {
            return Some(found);
        }
    }
    None
}
