//! The graph view the picker reads: plain per-task data from the task tree and the live run
//! state, with the depth-first order every picker rule uses.
//!
//! [`PickGraph::new`] takes plain [`PickTask`]s, so tests and other hosts can build one by hand;
//! [`from_work_tree`] builds one from a Work store snapshot. Nothing here is stored: the order,
//! the inherited areas and the chains are recomputed from the tasks every time.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bongos_work_model::derive::{self, Shape as WorkShape, State as WorkState};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{TodoState, UnblockPoint};

use crate::state::TaskId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shape {
    Group,
    Leaf,
}

/// A task's derived state, as the Work model derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskState {
    Cancelled,
    Done,
    Merged,
    Blocked,
    Ready,
    InProgress,
    Review,
    Implemented,
}

impl TaskState {
    /// Cancelled and Done tasks are finished.
    pub fn is_finished(self) -> bool {
        matches!(self, TaskState::Cancelled | TaskState::Done)
    }

    /// The name dispatch rules match in their `state` list.
    pub fn label(self) -> &'static str {
        match self {
            TaskState::Cancelled => "cancelled",
            TaskState::Done => "done",
            TaskState::Merged => "merged",
            TaskState::Blocked => "blocked",
            TaskState::Ready => "ready",
            TaskState::InProgress => "in-progress",
            TaskState::Review => "review",
            TaskState::Implemented => "implemented",
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    After,
    StackedOn,
}

/// One edge of a task to a sibling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickEdge {
    pub kind: EdgeKind,
    pub target: TaskId,
    /// `implemented`, `review` or `merged`.
    pub unblock: String,
    /// Whether the target has reached the unblock point.
    pub reached: bool,
    /// How far the target is, for the wait reason ("step 5 of 8 not done").
    pub progress: String,
}

/// The first todo still open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTodo {
    pub index: usize,
    pub name: String,
    pub workflow: Option<String>,
}

/// One task as the picker sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickTask {
    pub id: TaskId,
    /// The goal, for messages.
    pub name: String,
    /// The children in dependency order (the Work model's `child_order`).
    pub children: Vec<TaskId>,
    pub shape: Shape,
    pub state: TaskState,
    /// Edges to siblings, `after` edges first, then `stacked_on`.
    pub edges: Vec<PickEdge>,
    pub work_kind: Option<String>,
    /// The task's own area tags; [`PickGraph::areas`] adds every ancestor's.
    pub areas: Vec<String>,
    /// The size in jobs; unset counts as 1.
    pub size: Option<u32>,
    pub open_todo: Option<OpenTodo>,
    /// Whether any todo has started.
    pub started: bool,
    /// The active run on this task, from the host.
    pub active_run: Option<String>,
    /// Why the task is held, from the host.
    pub held: Option<String>,
}

impl PickTask {
    /// A leaf in state Ready with nothing else set.
    pub fn leaf(id: &str) -> Self {
        Self {
            id: TaskId::new(id),
            name: id.to_string(),
            children: Vec::new(),
            shape: Shape::Leaf,
            state: TaskState::Ready,
            edges: Vec::new(),
            work_kind: None,
            areas: Vec::new(),
            size: None,
            open_todo: None,
            started: false,
            active_run: None,
            held: None,
        }
    }

    /// A group holding `children`, in that order.
    pub fn group(id: &str, children: &[&str]) -> Self {
        Self {
            children: children.iter().map(|child| TaskId::new(*child)).collect(),
            shape: Shape::Group,
            ..Self::leaf(id)
        }
    }

    pub fn is_finished(&self) -> bool {
        self.state.is_finished()
    }
}

/// Live facts per task id from the host: the active run and why a task is held.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveFacts {
    pub active_runs: BTreeMap<TaskId, String>,
    pub held: BTreeMap<TaskId, String>,
}

/// The tasks of one project under its top group, walked depth first in child order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickGraph {
    tasks: BTreeMap<TaskId, PickTask>,
    top: Vec<TaskId>,
    /// Every task reachable from `top`, depth first.
    walk: Vec<TaskId>,
    /// 1-based position in `walk`.
    order: BTreeMap<TaskId, u32>,
    parent: BTreeMap<TaskId, TaskId>,
}

impl PickGraph {
    /// Builds the view from the top-level task ids in order and the tasks in any order. A child
    /// id with no task, or a task listed a second time, is skipped; tasks not reachable from
    /// `top` are kept but never walked.
    pub fn new(top: Vec<TaskId>, tasks: impl IntoIterator<Item = PickTask>) -> Self {
        let mut map = BTreeMap::new();
        for task in tasks {
            map.entry(task.id.clone()).or_insert(task);
        }
        let mut walk = Vec::new();
        let mut parent = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut kept_top = Vec::new();
        for id in &top {
            if map.contains_key(id) && seen.insert(id.clone()) {
                kept_top.push(id.clone());
                visit(&map, id, &mut walk, &mut parent, &mut seen);
            }
        }
        let order = walk
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index as u32 + 1))
            .collect();
        Self {
            tasks: map,
            top: kept_top,
            walk,
            order,
            parent,
        }
    }

    pub fn task(&self, id: &TaskId) -> Option<&PickTask> {
        self.tasks.get(id)
    }

    /// The top-level tasks in order.
    pub fn top(&self) -> &[TaskId] {
        &self.top
    }

    /// Every walked task, depth first.
    pub fn walk(&self) -> impl Iterator<Item = &PickTask> {
        self.walk.iter().filter_map(|id| self.tasks.get(id))
    }

    /// Every walked leaf, depth first.
    pub fn leaves(&self) -> impl Iterator<Item = &PickTask> {
        self.walk().filter(|task| task.shape == Shape::Leaf)
    }

    /// The 1-based depth-first position; tasks outside the walk come last.
    pub fn order(&self, id: &TaskId) -> u32 {
        self.order.get(id).copied().unwrap_or(u32::MAX)
    }

    pub fn parent(&self, id: &TaskId) -> Option<&TaskId> {
        self.parent.get(id)
    }

    /// `id` and its ancestors, from the top-level task down to `id`.
    pub fn path(&self, id: &TaskId) -> Vec<TaskId> {
        let mut path = vec![id.clone()];
        let mut current = id;
        while let Some(parent) = self.parent.get(current) {
            path.push(parent.clone());
            current = parent;
        }
        path.reverse();
        path
    }

    /// The top-level task above `id` (itself for a top-level task).
    pub fn top_level_of(&self, id: &TaskId) -> TaskId {
        self.path(id)
            .into_iter()
            .next()
            .unwrap_or_else(|| id.clone())
    }

    /// The tasks sharing `id`'s parent (the top-level tasks for a top-level task), `id` included.
    pub fn siblings(&self, id: &TaskId) -> Vec<TaskId> {
        match self.parent.get(id) {
            Some(parent) => self
                .tasks
                .get(parent)
                .map(|task| {
                    task.children
                        .iter()
                        .filter(|child| self.parent.get(*child) == Some(parent))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
            None => self.top.clone(),
        }
    }

    /// The siblings with an `after` or `stacked_on` edge to `id`, in order.
    pub fn dependents(&self, id: &TaskId) -> Vec<TaskId> {
        self.siblings(id)
            .into_iter()
            .filter(|sibling| sibling != id)
            .filter(|sibling| {
                self.tasks
                    .get(sibling)
                    .is_some_and(|task| task.edges.iter().any(|edge| &edge.target == id))
            })
            .collect()
    }

    /// The leaves under `id` (`id` itself for a leaf), depth first.
    pub fn leaves_under(&self, id: &TaskId) -> Vec<&PickTask> {
        let mut out = Vec::new();
        self.push_leaves(id, &mut out);
        out
    }

    fn push_leaves<'a>(&'a self, id: &TaskId, out: &mut Vec<&'a PickTask>) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        if task.shape == Shape::Leaf {
            out.push(task);
            return;
        }
        for child in &task.children {
            // Only the children the walk placed under this task, so a cycle cannot loop.
            if self.parent.get(child) == Some(id) {
                self.push_leaves(child, out);
            }
        }
    }

    /// Whether `id` or a leaf under it has started or finished.
    pub fn has_started_leaf(&self, id: &TaskId) -> bool {
        self.leaves_under(id)
            .iter()
            .any(|leaf| leaf.started || leaf.is_finished())
    }

    /// The task's area tags: its own and every ancestor's, sorted.
    pub fn areas(&self, id: &TaskId) -> BTreeSet<String> {
        self.path(id)
            .iter()
            .filter_map(|step| self.tasks.get(step))
            .flat_map(|task| task.areas.iter().cloned())
            .collect()
    }

    /// The area tags of `id` and every task under it.
    pub fn subtree_areas(&self, id: &TaskId) -> BTreeSet<String> {
        let mut areas = self.areas(id);
        let mut stack = vec![id.clone()];
        while let Some(current) = stack.pop() {
            if let Some(task) = self.tasks.get(&current) {
                areas.extend(task.areas.iter().cloned());
                stack.extend(
                    task.children
                        .iter()
                        .filter(|child| self.parent.get(*child) == Some(&current))
                        .cloned(),
                );
            }
        }
        areas
    }
}

fn visit(
    tasks: &BTreeMap<TaskId, PickTask>,
    id: &TaskId,
    walk: &mut Vec<TaskId>,
    parent: &mut BTreeMap<TaskId, TaskId>,
    seen: &mut BTreeSet<TaskId>,
) {
    walk.push(id.clone());
    let Some(task) = tasks.get(id) else {
        return;
    };
    for child in &task.children {
        if tasks.contains_key(child) && seen.insert(child.clone()) {
            parent.insert(child.clone(), id.clone());
            visit(tasks, child, walk, parent, seen);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// From the Work store
// ---------------------------------------------------------------------------------------------

/// Builds the view of the project whose top group has id (or slug) `project` from a Work store
/// snapshot and the host's live facts. `None` when no project matches. A task whose `task.toml`
/// never parsed has no id and is left out with everything under it.
pub fn from_work_tree(tree: &WorkTree, project: &str, live: &LiveFacts) -> Option<PickGraph> {
    let found = tree
        .projects
        .iter()
        .find(|candidate| candidate.task.id() == Some(project) || candidate.slug == project)?;
    let mut tasks = Vec::new();
    let top = derive::child_order(&found.task)
        .into_iter()
        .filter_map(|task| collect(task, &found.task, tree, live, &mut tasks))
        .collect();
    Some(PickGraph::new(top, tasks))
}

fn collect(
    task: &Task,
    parent: &Task,
    tree: &WorkTree,
    live: &LiveFacts,
    out: &mut Vec<PickTask>,
) -> Option<TaskId> {
    let file = task.file.as_ref()?;
    let id = TaskId::new(file.id.clone());
    let children = derive::child_order(task)
        .into_iter()
        .filter_map(|child| collect(child, task, tree, live, out))
        .collect();
    let shape = match derive::shape(task, tree) {
        WorkShape::Group => Shape::Group,
        WorkShape::Leaf => Shape::Leaf,
    };
    let open_todo = file
        .todos
        .iter()
        .enumerate()
        .find(|(_, todo)| todo.state == TodoState::Open)
        .map(|(index, todo)| OpenTodo {
            index,
            name: todo.name.clone(),
            workflow: todo.workflow.clone(),
        });
    out.push(PickTask {
        name: task.named_goal(),
        children,
        shape,
        state: task_state(derive::state(task, tree)),
        edges: edges(task, parent, tree),
        work_kind: file.work_kind.clone(),
        areas: file.areas.clone(),
        size: file.size,
        open_todo,
        started: file.todos.iter().any(|todo| todo.started.is_some()),
        active_run: live.active_runs.get(&id).cloned(),
        held: live.held.get(&id).cloned(),
        id: id.clone(),
    });
    Some(id)
}

fn task_state(state: WorkState) -> TaskState {
    match state {
        WorkState::Cancelled => TaskState::Cancelled,
        WorkState::Done => TaskState::Done,
        WorkState::Merged => TaskState::Merged,
        WorkState::Blocked => TaskState::Blocked,
        WorkState::Ready => TaskState::Ready,
        WorkState::InProgress => TaskState::InProgress,
        WorkState::Review => TaskState::Review,
        WorkState::Implemented => TaskState::Implemented,
    }
}

fn unblock_label(point: UnblockPoint) -> &'static str {
    match point {
        UnblockPoint::Implemented => "implemented",
        UnblockPoint::Review => "review",
        UnblockPoint::Merged => "merged",
    }
}

/// The task's edges to siblings that exist. Whether one is reached comes from the Work model's
/// `blocking_targets`: its own unreached targets are siblings, its ancestors' never are.
fn edges(task: &Task, parent: &Task, tree: &WorkTree) -> Vec<PickEdge> {
    let Some(file) = &task.file else {
        return Vec::new();
    };
    let unreached: Vec<&Task> = derive::blocking_targets(task, tree)
        .into_iter()
        .filter(|target| parent.children.iter().any(|kid| kid.dir == target.dir))
        .collect();
    let raw = file
        .after
        .iter()
        .map(|edge| (EdgeKind::After, edge.task.as_str(), edge.unblock))
        .chain(
            file.stacked_on
                .iter()
                .map(|edge| (EdgeKind::StackedOn, edge.task.as_str(), edge.unblock)),
        );
    raw.filter_map(|(kind, slug, unblock)| {
        let target = parent
            .children
            .iter()
            .find(|kid| kid.slug == slug && kid.dir != task.dir)?;
        let target_id = target.id()?;
        let reached = !unreached.iter().any(|other| other.dir == target.dir);
        Some(PickEdge {
            kind,
            target: TaskId::new(target_id),
            unblock: unblock_label(unblock).to_string(),
            reached,
            progress: if reached {
                String::new()
            } else {
                progress(target, unblock, tree)
            },
        })
    })
    .collect()
}

/// "step 5 of 8 not done" for a group target, "in-progress; needs merged" for a leaf.
fn progress(target: &Task, unblock: UnblockPoint, tree: &WorkTree) -> String {
    if derive::shape(target, tree) == WorkShape::Leaf {
        let state = task_state(derive::state(target, tree));
        return format!("{state}; needs {}", unblock_label(unblock));
    }
    let mut leaves = Vec::new();
    collect_leaves(target, tree, &mut leaves);
    let total = leaves.len();
    match leaves
        .iter()
        .position(|leaf| !task_state(derive::state(leaf, tree)).is_finished())
    {
        Some(index) => format!("step {} of {total} not done", index + 1),
        None => format!(
            "{total} of {total} steps done; needs {}",
            unblock_label(unblock)
        ),
    }
}

fn collect_leaves<'a>(task: &'a Task, tree: &WorkTree, out: &mut Vec<&'a Task>) {
    if derive::shape(task, tree) == WorkShape::Leaf {
        if derive::state(task, tree) != WorkState::Cancelled {
            out.push(task);
        }
        return;
    }
    for child in derive::child_order(task) {
        collect_leaves(child, tree, out);
    }
}
