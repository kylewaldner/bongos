//! The diff of a parsed proposal against the work tree: what the apply creates, changes, cancels,
//! moves and splits, what it keeps because a safety rule refuses it, and what it leaves untouched.
//! It is a pure function of the proposal, the tree, the running work and the keeper choices, so the
//! preview, the apply and the tests all read the same list.
//!
//! The safety rules come first. A done or cancelled task, and everything under it, is never
//! changed. A live task (a worktree, a branch, an open PR, a running chat or run, on it or under it)
//! may be renamed, gain todos and `after` edges, change its planned result, and have its open
//! unstarted todos renamed, removed or given another role. It is never cancelled, moved, given
//! another kind, stripped of an edge or given a `stacked on` edge. A frozen todo (closed,
//! started, or the current todo of a running chat or run) is never renamed, edited, removed or
//! reordered. A task the proposal does not mention is untouched; only a `drop` line cancels.
//!
//! Validation is a precondition: the proposal has no errors from [`crate::validate`]. The diff
//! stays total on any input, but it does not repeat those checks.

use std::collections::{BTreeMap, HashMap, HashSet};

use bongos_work_model::commands::{default_todos, default_unblock};
use bongos_work_model::derive::{self, Shape, State};
use bongos_work_model::layout::{CHATS_DIR, RUNS_DIR};
use bongos_work_model::rules::{EdgeKind, Place, check_split, open_descendants, role_name};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Extra, Kind, Status, TaskId, Todo, TodoRole, TodoState, UnblockPoint};
use sha2::{Digest, Sha256};

use crate::grammar::{self, Node, Parsed, kind_name};
use crate::print::LiveSet;

/// The choices the user makes between the preview and the apply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choices {
    /// For a split whose keeper the proposal does not mark: the label of the new child that keeps
    /// the work, by the id of the leaf being split. A label that names no new child is ignored.
    pub keepers: BTreeMap<TaskId, String>,
}

/// A task an operation names: one on disk, or one this proposal creates (by its task line).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskRef {
    Existing { id: TaskId, slug: String },
    New { line: usize, label: String },
}

impl TaskRef {
    /// The slug of an existing task, or the label of a new one.
    pub fn name(&self) -> &str {
        match self {
            TaskRef::Existing { slug, .. } => slug,
            TaskRef::New { label, .. } => label,
        }
    }
}

/// The file of a task a proposal creates, without what the apply decides (id, slug, time).
#[derive(Clone, Debug, PartialEq)]
pub struct NewTask {
    pub goal: String,
    pub kind: Kind,
    /// Empty for a new group.
    pub todos: Vec<Todo>,
    pub planned_result: Option<String>,
}

/// How the keeper of a split was chosen (D8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeeperSource {
    /// The proposal marks it `keeps work`.
    Marked,
    /// The user picked it ([`Choices::keepers`]).
    Chosen,
    /// The default: the first child with a todo named like the leaf's `produces-branch` todo, else
    /// the first new child.
    Default,
}

/// The new child that takes a split leaf's work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keeper {
    pub line: usize,
    pub label: String,
    pub source: KeeperSource,
}

/// What a leaf held when it was split, for the preview and the history.
#[derive(Clone, Debug, PartialEq)]
pub struct SplitWork {
    pub todos: Vec<Todo>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub result: bool,
    pub planned_result: bool,
    /// The chats under `chats/`.
    pub chats: usize,
    /// The runs under `runs/`.
    pub runs: usize,
}

/// One change to the todo list of an existing leaf. Indexes are 0-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TodoOp {
    /// A new todo at `index` of the new list. `after_frozen` names the last frozen todo when the
    /// proposal placed it before a frozen one and it was put after that one instead.
    Add {
        name: String,
        index: usize,
        after_frozen: Option<String>,
    },
    /// The todo at `index` of the old list gets a new name.
    Rename {
        index: usize,
        from: String,
        to: String,
    },
    SetRole {
        name: String,
        role: Option<TodoRole>,
    },
    SetWorkflow {
        name: String,
        workflow: Option<String>,
    },
    /// The todo at `index` of the old list goes.
    Remove { index: usize, name: String },
    /// The todo at `from` of the old list moves to `to` of the new list.
    Move {
        name: String,
        from: usize,
        to: usize,
    },
}

/// One operation of a diff. Every one but `Untouched` carries the 1-based proposal line it comes
/// from.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// A task line with no id.
    Create {
        line: usize,
        parent: TaskRef,
        label: String,
        task: NewTask,
    },
    /// The goal changes; the slug does not.
    Rename {
        line: usize,
        id: TaskId,
        from: String,
        to: String,
    },
    SetKind {
        line: usize,
        id: TaskId,
        from: Kind,
        kind: Kind,
    },
    /// `None` clears the planned result.
    SetPlannedResult {
        line: usize,
        id: TaskId,
        text: Option<String>,
    },
    /// The reconciled todo list of an existing leaf: `ops` say what changes, `todos` is the whole
    /// new list, with every frozen todo exactly as it is on disk.
    Todos {
        line: usize,
        id: TaskId,
        ops: Vec<TodoOp>,
        todos: Vec<Todo>,
    },
    /// An edge stored on `from`, naming the earlier sibling `target`. `default` marks the
    /// `after <keeper>` a fresh child of a split gains (D8).
    AddEdge {
        line: usize,
        from: TaskRef,
        target: TaskRef,
        kind: EdgeKind,
        unblock: UnblockPoint,
        default: bool,
    },
    /// An edge stored on the task `id`, naming its sibling by slug as on disk.
    RemoveEdge {
        line: usize,
        id: TaskId,
        target: String,
        kind: EdgeKind,
        unblock: UnblockPoint,
    },
    /// A `drop` line: the task is cancelled, and with it its open descendants `with`.
    Cancel {
        line: usize,
        id: TaskId,
        with: Vec<TaskId>,
    },
    /// The task, with its subtree, goes under `new_parent`. Its own edges go, and so do the edges
    /// its old siblings `unlinked` stored on it.
    Move {
        line: usize,
        id: TaskId,
        new_parent: TaskRef,
        unlinked: Vec<TaskId>,
    },
    /// An existing leaf becomes a group. With a keeper, the keeper (a `Create` of this diff) takes
    /// the leaf's work by feature 03's `split_work`, and `todo_ops` is the reconciliation of the
    /// leaf's todos with the keeper's lines. Without one, the leaf held no work: its todos go to
    /// its history and the new children are simply added.
    Split {
        line: usize,
        id: TaskId,
        keeper: Option<Keeper>,
        fresh: Vec<TaskRef>,
        work: SplitWork,
        todo_ops: Vec<TodoOp>,
    },
    /// The root's open `plans` todo is completed. `keeper` is set when the root is split and the
    /// todo moves into that new child; when the root becomes a group without one, the todo is
    /// recorded done in the root's history.
    PlanDone {
        line: usize,
        id: TaskId,
        todo: String,
        keeper: Option<TaskRef>,
    },
    /// Something the proposal asks that a safety rule refuses; shown, never blocking.
    Kept {
        id: TaskId,
        line: usize,
        what: String,
        reason: String,
    },
    /// An existing task under the root that no other operation names.
    Untouched { id: TaskId },
}

impl Op {
    /// The proposal line, `None` for `Untouched`.
    pub fn line(&self) -> Option<usize> {
        match self {
            Op::Create { line, .. }
            | Op::Rename { line, .. }
            | Op::SetKind { line, .. }
            | Op::SetPlannedResult { line, .. }
            | Op::Todos { line, .. }
            | Op::AddEdge { line, .. }
            | Op::RemoveEdge { line, .. }
            | Op::Cancel { line, .. }
            | Op::Move { line, .. }
            | Op::Split { line, .. }
            | Op::PlanDone { line, .. }
            | Op::Kept { line, .. } => Some(*line),
            Op::Untouched { .. } => None,
        }
    }

    /// The existing tasks whose files the apply writes for this operation, beyond the tasks it
    /// creates.
    fn writes(&self) -> Vec<&str> {
        match self {
            Op::Create { parent, .. } => match parent {
                TaskRef::Existing { id, .. } => vec![id.as_str()],
                TaskRef::New { .. } => Vec::new(),
            },
            Op::Rename { id, .. }
            | Op::SetKind { id, .. }
            | Op::SetPlannedResult { id, .. }
            | Op::Todos { id, .. }
            | Op::RemoveEdge { id, .. }
            | Op::Split { id, .. }
            | Op::PlanDone { id, .. } => vec![id.as_str()],
            Op::AddEdge { from, .. } => match from {
                TaskRef::Existing { id, .. } => vec![id.as_str()],
                TaskRef::New { .. } => Vec::new(),
            },
            Op::Cancel { id, with, .. } => std::iter::once(id.as_str())
                .chain(with.iter().map(String::as_str))
                .collect(),
            Op::Move {
                id,
                new_parent,
                unlinked,
                ..
            } => {
                let mut ids = vec![id.as_str()];
                if let TaskRef::Existing { id, .. } = new_parent {
                    ids.push(id.as_str());
                }
                ids.extend(unlinked.iter().map(String::as_str));
                ids
            }
            Op::Kept { .. } | Op::Untouched { .. } => Vec::new(),
        }
    }

    /// The existing tasks this operation names, for `Untouched`.
    fn names(&self) -> Vec<&str> {
        match self {
            Op::Kept { id, .. } => vec![id.as_str()],
            Op::Untouched { .. } => Vec::new(),
            Op::Create { .. } => Vec::new(),
            other => other.writes(),
        }
    }
}

/// The diff of a proposal: its operations in proposal-line order, then every `Untouched`.
#[derive(Clone, Debug, PartialEq)]
pub struct Diff {
    /// The id of the proposal's root task.
    pub root: TaskId,
    pub ops: Vec<Op>,
    /// Why the whole apply is refused, naming the file: a task the apply would write carries an
    /// error badge (feature 03 never writes such a task), or the root is not in the tree.
    pub refusal: Option<String>,
}

impl Diff {
    /// Whether applying would change nothing: every operation is `Untouched` and nothing refuses.
    pub fn is_empty(&self) -> bool {
        self.refusal.is_none() && self.ops.iter().all(|op| matches!(op, Op::Untouched { .. }))
    }
}

/// A stable fingerprint of what an apply would do: the SHA-256, in lowercase hex, over the ordered
/// operations, the keeper choices and the hash of the proposal file. The apply recomputes the diff
/// on the tree the writer hands it and refuses when this differs from the previewed one (D7).
///
/// The operations are written in their `Debug` form, which is fixed within one build; the preview
/// and the apply always run in the same process.
pub fn fingerprint(diff: &Diff, choices: &Choices, file_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bongos proposal diff 1\n");
    hasher.update(format!("root {:?}\n", diff.root).as_bytes());
    hasher.update(format!("refusal {:?}\n", diff.refusal).as_bytes());
    for op in &diff.ops {
        hasher.update(format!("{op:?}\n").as_bytes());
    }
    for (leaf, keeper) in &choices.keepers {
        hasher.update(format!("keeper {leaf:?} {keeper:?}\n").as_bytes());
    }
    hasher.update(format!("file {file_hash:?}\n").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The diff of `parsed` against `tree`, given the running work `live` and the user's `choices`.
pub fn diff(parsed: &Parsed, tree: &WorkTree, live: &LiveSet, choices: &Choices) -> Diff {
    let root_node = &parsed.root;
    let Some(root_id) = root_node.attributes.id.as_deref() else {
        return Diff {
            root: String::new(),
            ops: Vec::new(),
            refusal: Some("the root task has no id".to_string()),
        };
    };
    let infos = index(tree);
    let Some(root_info) = infos.get(root_id).copied() else {
        return Diff {
            root: root_id.to_string(),
            ops: Vec::new(),
            refusal: Some(format!("no task has the id {root_id}")),
        };
    };
    let in_root = root_info
        .task
        .walk()
        .into_iter()
        .filter_map(Task::id)
        .collect();
    let mut differ = Differ {
        tree,
        live,
        choices,
        infos,
        root: root_info.task,
        in_root,
        entries: Vec::new(),
        fates: Vec::new(),
        ops: Vec::new(),
        edge_ops: Vec::new(),
        wants_move: HashSet::new(),
        moved: HashSet::new(),
        cancelled: HashSet::new(),
        keepers: HashMap::new(),
        keeper_of: HashMap::new(),
        final_todos: HashMap::new(),
        removed_edges: HashSet::new(),
        plan_todo: None,
    };
    differ.flatten(root_node, None);
    differ.find_moves();
    differ.find_drops();
    for index in 0..differ.entries.len() {
        differ.decide(index);
    }
    differ.edges();
    let mut ops = differ.ops;
    ops.append(&mut differ.edge_ops);
    ops.sort_by_key(|op| op.line().unwrap_or(usize::MAX));
    let named: HashSet<&str> = ops.iter().flat_map(Op::names).collect();
    let untouched: Vec<Op> = root_info
        .task
        .walk()
        .into_iter()
        .filter_map(Task::id)
        .filter(|id| !named.contains(id))
        .map(|id| Op::Untouched { id: id.to_string() })
        .collect();
    ops.extend(untouched);
    let refusal = badge_refusal(&ops, root_info.task, &differ.infos);
    Diff {
        root: root_id.to_string(),
        ops,
        refusal,
    }
}

/// The first task the apply would write or move that carries an error badge, as a refusal naming
/// its file. A move renames the whole subtree, so every task under a moved one counts.
fn badge_refusal(ops: &[Op], root: &Task, infos: &HashMap<&str, Info<'_>>) -> Option<String> {
    let mut written = vec![root];
    for op in ops {
        written.extend(
            op.writes()
                .into_iter()
                .filter_map(|id| Some(infos.get(id)?.task)),
        );
        if let Op::Move { id, .. } = op
            && let Some(info) = infos.get(id.as_str())
        {
            written.extend(info.task.walk().into_iter().skip(1));
        }
    }
    written.into_iter().find_map(|task| {
        task.check_writable()
            .err()
            .map(|refused| refused.to_string())
    })
}

/// A task of the tree with its parent on disk and, when it or an ancestor is done or cancelled,
/// the word for that.
#[derive(Clone, Copy)]
struct Info<'a> {
    task: &'a Task,
    parent: Option<&'a Task>,
    closed: Option<&'static str>,
}

fn index(tree: &WorkTree) -> HashMap<&str, Info<'_>> {
    fn visit<'a>(
        task: &'a Task,
        parent: Option<&'a Task>,
        inherited: Option<&'static str>,
        tree: &'a WorkTree,
        infos: &mut HashMap<&'a str, Info<'a>>,
    ) {
        let own = match derive::state(task, tree) {
            State::Done => Some("done"),
            State::Cancelled => Some("cancelled"),
            _ => None,
        };
        let closed = own.or(inherited);
        if let Some(id) = task.id() {
            infos.insert(
                id,
                Info {
                    task,
                    parent,
                    closed,
                },
            );
        }
        for child in &task.children {
            visit(child, Some(task), closed, tree, infos);
        }
    }

    let mut infos = HashMap::new();
    for project in &tree.projects {
        visit(&project.task, None, None, tree, &mut infos);
    }
    for activity in &tree.activities {
        for oneoff in &activity.oneoffs {
            visit(oneoff, None, None, tree, &mut infos);
        }
    }
    infos
}

/// A task of the sibling-set graphs: on disk, new (by its line), or a broken edge's slug.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    Existing(TaskId),
    New(usize),
    Broken(String),
}

/// One task line of the proposal, in file order (depth first).
#[derive(Clone, Copy)]
struct Entry<'a> {
    node: &'a Node,
    parent: Option<usize>,
    /// The task on disk under the root that the line's id names.
    task: Option<&'a Task>,
    /// The line has an id that names no task under the root.
    unknown: bool,
}

/// Where a task line ends up after the apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spot {
    /// At its place in the proposal: created, existing in place, or moved there.
    Here,
    /// Existing and open, but left where it is on disk because its move is refused.
    Stayed,
    /// Done or cancelled, or under such a task: nothing is asked of it.
    Closed,
    /// Cancelled by this proposal, or under a task that is.
    Dropped,
    /// Not in the tree after the apply: a new task that is not created, or an unknown id.
    Absent,
}

/// Why the children of a task line cannot be placed under it, and which task says so.
#[derive(Clone, Debug)]
struct Block {
    id: TaskId,
    reason: String,
}

#[derive(Clone, Debug)]
struct Fate {
    spot: Spot,
    holds: Result<(), Block>,
}

/// What the keeper of a split is created with.
struct KeeperPlan {
    todos: Vec<Todo>,
    kind: Kind,
    planned_result: Option<String>,
}

/// An edge the proposal adds, before the cycle and stack checks.
struct Candidate {
    line: usize,
    group: Key,
    from: (Key, TaskRef),
    target: (Key, TaskRef),
    kind: EdgeKind,
    unblock: UnblockPoint,
    text: String,
    anchor: TaskId,
}

struct Differ<'a> {
    tree: &'a WorkTree,
    live: &'a LiveSet,
    choices: &'a Choices,
    infos: HashMap<&'a str, Info<'a>>,
    root: &'a Task,
    in_root: HashSet<&'a str>,
    entries: Vec<Entry<'a>>,
    fates: Vec<Fate>,
    ops: Vec<Op>,
    edge_ops: Vec<Op>,
    /// Existing tasks the proposal places under another parent.
    wants_move: HashSet<&'a str>,
    /// Existing tasks whose move is accepted.
    moved: HashSet<&'a str>,
    /// Existing tasks this proposal cancels.
    cancelled: HashSet<&'a str>,
    /// By the keeper's entry.
    keepers: HashMap<usize, KeeperPlan>,
    /// The keeper's entry, by the split leaf's entry.
    keeper_of: HashMap<usize, usize>,
    /// Every leaf's todo list after the apply, for the stack check.
    final_todos: HashMap<Key, Vec<Todo>>,
    /// The accepted removals: (stored on, kind, target).
    removed_edges: HashSet<(Key, EdgeKind, Key)>,
    /// The root's open `plans` todo, by index, when this proposal completes it.
    plan_todo: Option<usize>,
}

impl<'a> Differ<'a> {
    fn flatten(&mut self, node: &'a Node, parent: Option<usize>) {
        let (task, unknown) = match &node.attributes.id {
            None => (None, false),
            Some(id) => match self.infos.get(id.as_str()) {
                Some(info) if self.in_root.contains(id.as_str()) => (Some(info.task), false),
                _ => (None, true),
            },
        };
        let index = self.entries.len();
        self.entries.push(Entry {
            node,
            parent,
            task,
            unknown,
        });
        for child in &node.children {
            self.flatten(child, Some(index));
        }
    }

    /// Every existing task the proposal places under a parent other than its parent on disk.
    fn find_moves(&mut self) {
        for entry in &self.entries {
            let (Some(task), Some(parent)) = (entry.task, entry.parent) else {
                continue;
            };
            let Some(id) = task.id() else { continue };
            let disk_parent = self.infos.get(id).and_then(|info| info.parent);
            let same = match (self.entries[parent].task, disk_parent) {
                (Some(proposed), Some(disk)) => proposed.dir == disk.dir,
                _ => false,
            };
            if !same {
                self.wants_move.insert(id);
            }
        }
    }

    /// Every open task with a `drop` line that is not live, before any other decision, so a task
    /// under a dropped one is seen as dropped wherever the proposal lists it.
    fn find_drops(&mut self) {
        let drops: Vec<&'a str> = self
            .entries
            .iter()
            .filter(|entry| entry.node.attributes.drop)
            .filter_map(|entry| entry.task)
            .filter(|task| {
                task.id()
                    .and_then(|id| self.infos.get(id))
                    .is_some_and(|info| info.closed.is_none())
                    && self.live_reason(task).is_none()
            })
            .filter_map(Task::id)
            .collect();
        self.cancelled.extend(drops);
    }

    fn key(&self, index: usize) -> Key {
        let entry = &self.entries[index];
        match entry.task.and_then(Task::id) {
            Some(id) => Key::Existing(id.to_string()),
            None => Key::New(entry.node.line),
        }
    }

    fn task_ref(&self, index: usize) -> TaskRef {
        let entry = &self.entries[index];
        match entry.task {
            Some(task) => TaskRef::Existing {
                id: task.id().unwrap_or_default().to_string(),
                slug: task.slug.clone(),
            },
            None => TaskRef::New {
                line: entry.node.line,
                label: entry.node.label.clone(),
            },
        }
    }

    fn children_of(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        (0..self.entries.len()).filter(move |&child| self.entries[child].parent == Some(index))
    }

    /// The id of the nearest task line at or above `index` that names a task on disk.
    fn anchor_id(&self, index: usize) -> TaskId {
        let mut at = Some(index);
        while let Some(current) = at {
            let entry = &self.entries[current];
            if let Some(id) = entry.task.and_then(Task::id) {
                return id.to_string();
            }
            if entry.unknown
                && let Some(id) = &entry.node.attributes.id
            {
                return id.clone();
            }
            at = entry.parent;
        }
        self.root.id().unwrap_or_default().to_string()
    }

    fn kept(
        &mut self,
        id: impl Into<String>,
        line: usize,
        what: impl Into<String>,
        reason: impl Into<String>,
    ) {
        self.ops.push(Op::Kept {
            id: id.into(),
            line,
            what: what.into(),
            reason: reason.into(),
        });
    }

    /// Why `task` is live, on it or (for a group) under it, or `None`. A done or cancelled
    /// descendant counts only for a running chat or run: its old branch is no running work.
    fn live_reason(&self, task: &Task) -> Option<String> {
        let mut reasons: Vec<String> = self
            .live
            .sources(task)
            .into_iter()
            .map(|source| source.reason().to_string())
            .collect();
        for descendant in task.walk().into_iter().skip(1) {
            let closed = descendant
                .id()
                .and_then(|id| self.infos.get(id))
                .is_some_and(|info| info.closed.is_some());
            for source in self.live.sources(descendant) {
                use crate::print::LiveSource;
                let counts = !closed
                    || matches!(
                        source,
                        LiveSource::Chat | LiveSource::Run | LiveSource::Running
                    );
                if counts {
                    reasons.push(format!("'{}' {}", descendant.slug, source.reason()));
                }
            }
        }
        (!reasons.is_empty()).then(|| reasons.join(", "))
    }

    /// Why each todo of `task` is frozen, or `None` for an open, unstarted todo that is not the
    /// current todo of a running chat or run. With `plan`, the open `plans` todo this proposal
    /// completes is frozen too.
    fn frozen(&self, task: &Task, plan: Option<usize>) -> Vec<Option<&'static str>> {
        let Some(file) = &task.file else {
            return Vec::new();
        };
        let running = task.id().is_some_and(|id| self.live.has_running_work(id));
        let current = file
            .todos
            .iter()
            .position(|todo| todo.state == TodoState::Open);
        file.todos
            .iter()
            .enumerate()
            .map(|(index, todo)| match todo.state {
                TodoState::Open if plan == Some(index) => Some(PLAN_TODO),
                TodoState::Done => Some("done"),
                TodoState::Skipped => Some("skipped"),
                TodoState::Cancelled => Some("cancelled"),
                TodoState::Open if todo.started.is_some() => Some("started"),
                TodoState::Open if running && current == Some(index) => Some("running"),
                TodoState::Open => None,
            })
            .collect()
    }

    fn decide(&mut self, index: usize) {
        let entry = self.entries[index];
        let parent_holds = match entry.parent {
            Some(parent) => self.fates[parent].holds.clone(),
            None => Ok(()),
        };
        let fate = if entry.unknown {
            let id = entry.node.attributes.id.clone().unwrap_or_default();
            let reason = "no task under the root has this id".to_string();
            self.kept(id.clone(), entry.node.line, "not changed", reason.clone());
            Fate {
                spot: Spot::Absent,
                holds: Err(Block { id, reason }),
            }
        } else {
            match entry.task {
                None => self.decide_new(index, parent_holds),
                Some(task) => self.decide_existing(index, task, parent_holds),
            }
        };
        self.fates.push(fate);
    }

    fn decide_new(&mut self, index: usize, parent_holds: Result<(), Block>) -> Fate {
        let entry = self.entries[index];
        let node = entry.node;
        let not_created = format!("'{}' not created", node.label);
        if let Err(block) = parent_holds {
            self.kept(
                block.id.clone(),
                node.line,
                not_created,
                block.reason.clone(),
            );
            return Fate {
                spot: Spot::Absent,
                holds: Err(block),
            };
        }
        if node.attributes.drop {
            let id = self.anchor_id(index);
            self.kept(
                id.clone(),
                node.line,
                not_created,
                "a new task cannot be dropped",
            );
            return Fate {
                spot: Spot::Absent,
                holds: Err(Block {
                    id,
                    reason: format!("'{}' is not created", node.label),
                }),
            };
        }
        let Some(parent) = entry.parent else {
            // Only the root has no parent, and the root always names a task.
            return Fate {
                spot: Spot::Absent,
                holds: Err(Block {
                    id: self.anchor_id(index),
                    reason: "no parent".to_string(),
                }),
            };
        };
        let group = !node.children.is_empty();
        let task = match self.keepers.remove(&index) {
            Some(plan) => NewTask {
                goal: node.goal.clone(),
                kind: plan.kind,
                todos: plan.todos,
                planned_result: plan.planned_result,
            },
            None => {
                let kind = node.attributes.kind.unwrap_or(Kind::PrShip);
                NewTask {
                    goal: node.goal.clone(),
                    kind,
                    todos: if group {
                        Vec::new()
                    } else {
                        new_todos(&node.todos, kind)
                    },
                    planned_result: if group {
                        None
                    } else {
                        node.results.first().map(|result| result.value.clone())
                    },
                }
            }
        };
        if !group {
            self.final_todos
                .insert(Key::New(node.line), task.todos.clone());
        }
        self.ops.push(Op::Create {
            line: node.line,
            parent: self.task_ref(parent),
            label: node.label.clone(),
            task,
        });
        Fate {
            spot: Spot::Here,
            holds: Ok(()),
        }
    }

    fn decide_existing(
        &mut self,
        index: usize,
        task: &'a Task,
        parent_holds: Result<(), Block>,
    ) -> Fate {
        let entry = self.entries[index];
        let node = entry.node;
        let line = node.line;
        let (Some(file), Some(id)) = (&task.file, task.id()) else {
            return Fate {
                spot: Spot::Absent,
                holds: Err(Block {
                    id: self.anchor_id(index),
                    reason: format!("'{}' has no task file", task.slug),
                }),
            };
        };
        let Some(info) = self.infos.get(id).copied() else {
            return Fate {
                spot: Spot::Absent,
                holds: Err(Block {
                    id: id.to_string(),
                    reason: "not in the tree".to_string(),
                }),
            };
        };
        let wants_move = self.wants_move.contains(id);
        let new_parent_name = entry
            .parent
            .map(|parent| self.task_ref(parent).name().to_string())
            .unwrap_or_default();

        if let Some(word) = info.closed {
            self.closed_asks(index, task, word, wants_move, &new_parent_name);
            return Fate {
                spot: Spot::Closed,
                holds: Err(Block {
                    id: id.to_string(),
                    reason: format!("'{}' is {word}", task.slug),
                }),
            };
        }

        if let Some(dropped) = self.dropped_ancestor(info) {
            let reason = format!("'{}' is dropped", dropped.slug);
            if wants_move {
                self.kept(
                    id,
                    line,
                    format!("not moved under '{new_parent_name}'"),
                    reason.clone(),
                );
            }
            return Fate {
                spot: Spot::Dropped,
                holds: Err(Block {
                    id: id.to_string(),
                    reason,
                }),
            };
        }

        if node.attributes.drop {
            match self.live_reason(task) {
                None if self.cancelled.contains(id) => {
                    // A task under a done or cancelled one is never written (D6).
                    let with = open_descendants(task, self.tree)
                        .into_iter()
                        .filter_map(Task::id)
                        .filter(|id| self.infos.get(id).is_some_and(|info| info.closed.is_none()))
                        .map(str::to_string)
                        .collect();
                    self.ops.push(Op::Cancel {
                        line,
                        id: id.to_string(),
                        with,
                    });
                    return Fate {
                        spot: Spot::Dropped,
                        holds: Err(Block {
                            id: id.to_string(),
                            reason: format!("'{}' is dropped", task.slug),
                        }),
                    };
                }
                None => {}
                Some(reason) => self.kept(id, line, "not removed", reason),
            }
        }

        let mut spot = Spot::Here;
        if wants_move && let Some(parent) = entry.parent {
            match self.move_refusal(index, task, info, &parent_holds) {
                None => {
                    let unlinked = self
                        .linked_siblings(task, info)
                        .into_iter()
                        .filter_map(Task::id)
                        .map(str::to_string)
                        .collect();
                    self.ops.push(Op::Move {
                        line,
                        id: id.to_string(),
                        new_parent: self.task_ref(parent),
                        unlinked,
                    });
                    self.moved.insert(id);
                }
                Some(reason) => {
                    self.kept(
                        id,
                        line,
                        format!("not moved under '{new_parent_name}'"),
                        reason,
                    );
                    spot = Spot::Stayed;
                }
            }
        }

        if !same(&node.goal, &file.goal) {
            self.ops.push(Op::Rename {
                line,
                id: id.to_string(),
                from: file.goal.clone(),
                to: node.goal.clone(),
            });
        }

        if let Some(kind) = node.attributes.kind
            && kind != file.kind
        {
            match self.live_reason(task) {
                None => self.ops.push(Op::SetKind {
                    line,
                    id: id.to_string(),
                    from: file.kind,
                    kind,
                }),
                Some(reason) => self.kept(
                    id,
                    line,
                    format!("kind not changed to {}", kind_name(kind)),
                    reason,
                ),
            }
        }

        if index == 0 {
            self.plan_todo = file.todos.iter().position(|todo| {
                todo.role == Some(TodoRole::Plans) && todo.state == TodoState::Open
            });
        }
        let plan = if index == 0 { self.plan_todo } else { None };

        let disk_group = derive::shape(task, self.tree) == Shape::Group;
        let mut holds = Ok(());
        let mut plan_keeper = None;
        if !node.children.is_empty() {
            if !disk_group {
                holds = self.split(index, task, plan);
                if let Some(&keeper) = self.keeper_of.get(&index) {
                    plan_keeper = Some(self.task_ref(keeper));
                }
            }
        } else if disk_group {
            if let Some(first) = node
                .results
                .first()
                .map(|result| result.line)
                .or_else(|| node.todos.first().map(|todo| todo.line))
            {
                self.kept(
                    id,
                    first,
                    "todos and result not set",
                    "a group has no todos or result",
                );
            }
        } else {
            // A line with no `result` keeps the planned result on disk, as one with no todos keeps
            // the todos: a terse rename must not clear it.
            if let Some(result) = node.results.first()
                && !same_opt(Some(result.value.as_str()), file.planned_result.as_deref())
            {
                self.ops.push(Op::SetPlannedResult {
                    line: result.line,
                    id: id.to_string(),
                    text: Some(result.value.clone()),
                });
            }
            let todos = if node.todos.is_empty() {
                file.todos.clone()
            } else {
                let frozen = self.frozen(task, plan);
                // The creation rule of D3 also reads lines written without roles here, so a
                // proposal applied once diffs to nothing when applied again.
                let kind = node.attributes.kind.unwrap_or(file.kind);
                let lines = with_creation_roles(&node.todos, kind, &file.todos);
                let reconciled = reconcile(&file.todos, &frozen, &lines, line);
                for (kept_line, what, reason) in reconciled.kept {
                    self.kept(id, kept_line, what, reason);
                }
                if !reconciled.ops.is_empty() {
                    self.ops.push(Op::Todos {
                        line: node.todos.first().map_or(line, |todo| todo.line),
                        id: id.to_string(),
                        ops: reconciled.ops,
                        todos: reconciled.todos.clone(),
                    });
                }
                reconciled.todos
            };
            self.final_todos
                .insert(Key::Existing(id.to_string()), todos);
        }

        if let Some(plan) = plan
            && holds.is_ok()
            && let Some(todo) = file.todos.get(plan)
        {
            self.ops.push(Op::PlanDone {
                line,
                id: id.to_string(),
                todo: todo.name.clone(),
                keeper: plan_keeper,
            });
        }

        Fate { spot, holds }
    }

    /// The asks of a proposal line on a done or cancelled task, each kept with the word. A task
    /// printed as one line (no result, todo, edge or child lines) asks nothing of its body.
    fn closed_asks(
        &mut self,
        index: usize,
        task: &Task,
        word: &'static str,
        wants_move: bool,
        new_parent: &str,
    ) {
        let node = self.entries[index].node;
        let (Some(file), Some(id)) = (&task.file, task.id()) else {
            return;
        };
        let line = node.line;
        if wants_move {
            self.kept(id, line, format!("not moved under '{new_parent}'"), word);
        }
        if node.attributes.drop && file.status != Status::Cancelled {
            self.kept(id, line, "not removed", word);
        }
        if !same(&node.goal, &file.goal) {
            self.kept(id, line, format!("not renamed to '{}'", node.goal), word);
        }
        if let Some(kind) = node.attributes.kind
            && kind != file.kind
        {
            self.kept(
                id,
                line,
                format!("kind not changed to {}", kind_name(kind)),
                word,
            );
        }
        let collapsed = node.todos.is_empty()
            && node.results.is_empty()
            && node.edges.is_empty()
            && node.children.is_empty();
        if collapsed {
            return;
        }
        let leaf = derive::shape(task, self.tree) == Shape::Leaf;
        if leaf && !node.children.is_empty() {
            self.kept(id, line, "not split", word);
        }
        // A body part with no lines asks nothing: it may have been left out for brevity.
        if leaf && node.children.is_empty() {
            let proposed = node.results.first().map(|result| result.value.as_str());
            if proposed.is_some() && !same_opt(proposed, file.planned_result.as_deref()) {
                let at = node.results.first().map_or(line, |result| result.line);
                self.kept(id, at, "result not changed", word);
            }
            let same_todos = node.todos.len() == file.todos.len()
                && node.todos.iter().zip(&file.todos).all(|(line, todo)| {
                    same(&line.name, &todo.name)
                        && line.role == todo.role
                        && same_opt(line.workflow.as_deref(), todo.workflow.as_deref())
                });
            if !node.todos.is_empty() && !same_todos {
                let at = node.todos.first().map_or(line, |todo| todo.line);
                self.kept(id, at, "todos not changed", word);
            }
        }
        if index != 0 && !self.edges_as_on_disk(index, task) {
            let at = node.edges.first().map_or(line, |edge| edge.line);
            self.kept(id, at, "edges not changed", word);
        }
    }

    /// Whether the edge lines of an existing task's line say what its file says, matching
    /// targets by slug.
    fn edges_as_on_disk(&self, index: usize, task: &Task) -> bool {
        let Some(file) = &task.file else {
            return true;
        };
        let node = self.entries[index].node;
        let mut proposed: Vec<(bool, String, UnblockPoint)> = node
            .edges
            .iter()
            .map(|edge| {
                let label = edge.target.join("/");
                let slug = edge
                    .target
                    .first()
                    .filter(|_| edge.target.len() == 1)
                    .and_then(|label| self.sibling_slug(index, label))
                    .unwrap_or(label);
                (
                    edge.kind == EdgeKind::StackedOn,
                    slug,
                    edge.unblock.unwrap_or_else(|| default_unblock(edge.kind)),
                )
            })
            .collect();
        let mut disk: Vec<(bool, String, UnblockPoint)> = disk_edges(file)
            .into_iter()
            .map(|(kind, slug, unblock)| (kind == EdgeKind::StackedOn, slug, unblock))
            .collect();
        let order =
            |edge: &(bool, String, UnblockPoint)| (edge.0, edge.1.clone(), format!("{:?}", edge.2));
        proposed.sort_by_key(order);
        disk.sort_by_key(order);
        proposed == disk
    }

    /// The slug of the sibling a label names in the proposal, when it is an existing task.
    fn sibling_slug(&self, index: usize, label: &str) -> Option<String> {
        let parent = self.entries[index].parent?;
        self.children_of(parent)
            .find(|&sibling| self.entries[sibling].node.label == label)
            .and_then(|sibling| self.entries[sibling].task)
            .map(|task| task.slug.clone())
    }

    /// The task this proposal cancels that `info`'s task is under on disk.
    fn dropped_ancestor(&self, info: Info<'a>) -> Option<&'a Task> {
        let mut at = info.parent;
        while let Some(task) = at {
            let id = task.id()?;
            if self.cancelled.contains(id) {
                return Some(task);
            }
            at = self.infos.get(id).and_then(|info| info.parent);
        }
        None
    }

    /// The old siblings whose edges name `task`, which a move removes.
    fn linked_siblings(&self, task: &Task, info: Info<'a>) -> Vec<&'a Task> {
        let Some(parent) = info.parent else {
            return Vec::new();
        };
        parent
            .children
            .iter()
            .filter(|sibling| sibling.dir != task.dir)
            .filter(|sibling| {
                sibling.file.as_ref().is_some_and(|file| {
                    disk_edges(file)
                        .iter()
                        .any(|(_, target, _)| *target == task.slug)
                })
            })
            .collect()
    }

    /// Why moving the task at `index` under its proposal parent is refused, or `None`.
    fn move_refusal(
        &self,
        index: usize,
        task: &Task,
        info: Info<'a>,
        parent_holds: &Result<(), Block>,
    ) -> Option<String> {
        if let Err(block) = parent_holds {
            return Some(block.reason.clone());
        }
        // One level of moves per proposal: nothing above it, in the proposal or on disk, moves.
        let mut at = self.entries[index].parent;
        while let Some(current) = at {
            if let Some(ancestor) = self.entries[current].task
                && ancestor.id().is_some_and(|id| self.wants_move.contains(id))
            {
                return Some(format!(
                    "'{}' also moves; apply, then propose again",
                    ancestor.slug
                ));
            }
            at = self.entries[current].parent;
        }
        let mut disk = info.parent;
        while let Some(ancestor) = disk {
            let Some(id) = ancestor.id() else { break };
            if self.wants_move.contains(id) {
                return Some(format!(
                    "'{}' also moves; apply, then propose again",
                    ancestor.slug
                ));
            }
            disk = self.infos.get(id).and_then(|info| info.parent);
        }
        if let Some(reason) = self.live_reason(task) {
            return Some(reason);
        }
        // The nearest task on disk above its new place must not be inside it.
        let mut at = self.entries[index].parent;
        while let Some(current) = at {
            if let Some(anchor) = self.entries[current].task {
                if task.walk().iter().any(|inner| inner.dir == anchor.dir) {
                    return Some(format!("'{}' is inside it", anchor.slug));
                }
                break;
            }
            at = self.entries[current].parent;
        }
        for sibling in self.linked_siblings(task, info) {
            let closed = sibling
                .id()
                .and_then(|id| self.infos.get(id))
                .and_then(|info| info.closed);
            if let Some(word) = closed {
                return Some(format!(
                    "'{}' has an edge to it and is {word}",
                    sibling.slug
                ));
            }
            if let Some(reason) = self.live_reason(sibling) {
                return Some(format!("'{}' has an edge to it and {reason}", sibling.slug));
            }
        }
        None
    }

    /// An existing leaf whose line has children (D8). Returns whether children can be placed
    /// under it.
    fn split(&mut self, index: usize, task: &'a Task, plan: Option<usize>) -> Result<(), Block> {
        let node = self.entries[index].node;
        let line = node.line;
        let (Some(file), Some(id)) = (&task.file, task.id()) else {
            return Ok(());
        };
        let refuse = |reason: String| Block {
            id: id.to_string(),
            reason,
        };
        if let Ok(place) = Place::locate(self.tree, id)
            && let Err(message) = check_split(&place, self.tree)
        {
            self.kept(id, line, "not split", message.clone());
            return Err(refuse(message));
        }
        let frozen = self.frozen(task, plan);
        let has_work = file.worktree.is_some()
            || file.branch.is_some()
            || file.result.is_some()
            || file
                .todos
                .iter()
                .zip(&frozen)
                .any(|(todo, reason)| reason.is_some() && todo.role != Some(TodoRole::Plans));
        let new_children: Vec<usize> = self
            .children_of(index)
            .filter(|&child| {
                let entry = &self.entries[child];
                entry.task.is_none() && !entry.unknown && !entry.node.attributes.drop
            })
            .collect();
        let work = split_work(task);

        if !has_work {
            let fresh = new_children
                .iter()
                .map(|&child| self.task_ref(child))
                .collect();
            self.ops.push(Op::Split {
                line,
                id: id.to_string(),
                keeper: None,
                fresh,
                work,
                todo_ops: Vec::new(),
            });
            return Ok(());
        }
        let running = if self.live.chats.contains(id) {
            Some("has a running chat; close it first")
        } else if self.live.runs.contains(id) {
            Some("has a running run; close it first")
        } else if self.live.running.contains(id) {
            Some("has running work; close it first")
        } else {
            None
        };
        if let Some(reason) = running {
            self.kept(id, line, "not split", reason);
            return Err(refuse(reason.to_string()));
        }
        let leaves: Vec<usize> = new_children
            .iter()
            .copied()
            .filter(|&child| self.entries[child].node.children.is_empty())
            .collect();
        if leaves.is_empty() {
            let reason = "its work moves into a new leaf child; add one without an id".to_string();
            self.kept(id, line, "not split", reason.clone());
            return Err(refuse(reason));
        }

        let (keeper, source) = self.choose_keeper(id, file, &leaves);
        let keeper_node = self.entries[keeper].node;
        let kind = keeper_node.attributes.kind.unwrap_or(file.kind);
        let (todos, todo_ops) = if keeper_node.todos.is_empty() {
            (file.todos.clone(), Vec::new())
        } else {
            let lines = with_creation_roles(&keeper_node.todos, kind, &file.todos);
            let reconciled = reconcile(&file.todos, &frozen, &lines, keeper_node.line);
            for (kept_line, what, reason) in reconciled.kept {
                self.kept(id, kept_line, what, reason);
            }
            (reconciled.todos, reconciled.ops)
        };
        self.keepers.insert(
            keeper,
            KeeperPlan {
                todos,
                kind,
                planned_result: keeper_node
                    .results
                    .first()
                    .map(|result| result.value.clone())
                    .or_else(|| file.planned_result.clone()),
            },
        );
        self.keeper_of.insert(index, keeper);
        let fresh = new_children
            .iter()
            .filter(|&&child| child != keeper)
            .map(|&child| self.task_ref(child))
            .collect();
        self.ops.push(Op::Split {
            line,
            id: id.to_string(),
            keeper: Some(Keeper {
                line: keeper_node.line,
                label: keeper_node.label.clone(),
                source,
            }),
            fresh,
            work,
            todo_ops,
        });
        Ok(())
    }

    /// D8: the child marked `keeps work`; else the user's choice; else the first child whose todos
    /// include one named like the leaf's `produces-branch` todo; else the first new child.
    fn choose_keeper(
        &self,
        id: &str,
        file: &bongos_work_model::TaskFile,
        new_children: &[usize],
    ) -> (usize, KeeperSource) {
        let node = |child: usize| self.entries[child].node;
        if let Some(&child) = new_children
            .iter()
            .find(|&&child| node(child).attributes.keeps_work)
        {
            return (child, KeeperSource::Marked);
        }
        if let Some(label) = self.choices.keepers.get(id)
            && let Some(&child) = new_children
                .iter()
                .find(|&&child| node(child).label == *label)
        {
            return (child, KeeperSource::Chosen);
        }
        let produces = file
            .todos
            .iter()
            .find(|todo| todo.role == Some(TodoRole::ProducesBranch));
        if let Some(produces) = produces
            && let Some(&child) = new_children.iter().find(|&&child| {
                let child = node(child);
                let kind = child.attributes.kind.unwrap_or(file.kind);
                if child.todos.is_empty() {
                    default_todos(kind)
                        .iter()
                        .any(|todo| same(&todo.name, &produces.name))
                } else {
                    child
                        .todos
                        .iter()
                        .any(|todo| same(&todo.name, &produces.name))
                }
            })
        {
            return (child, KeeperSource::Default);
        }
        (
            new_children.first().copied().unwrap_or_default(),
            KeeperSource::Default,
        )
    }

    /// The effective parent of a task line after the apply, `None` when it is not in the tree.
    fn effective_parent(&self, index: usize) -> Option<Key> {
        let entry = &self.entries[index];
        match self.fates[index].spot {
            Spot::Here => entry.parent.map(|parent| self.key(parent)),
            Spot::Stayed | Spot::Closed | Spot::Dropped => {
                let id = entry.task?.id()?;
                let parent = self.infos.get(id)?.parent?;
                Some(Key::Existing(parent.id()?.to_string()))
            }
            Spot::Absent => None,
        }
    }

    /// The sibling a label names for the task line at `index`, as it will be after the apply.
    fn resolve(&self, index: usize, label: &str) -> Result<(Key, TaskRef), String> {
        let Some(parent) = self.entries[index].parent else {
            return Err(format!("unknown task '{label}'"));
        };
        let own = self.effective_parent(index);
        if let Some(sibling) = self
            .children_of(parent)
            .find(|&sibling| self.entries[sibling].node.label == label)
        {
            return match (self.entries[sibling].task, self.fates[sibling].spot) {
                (None, Spot::Here) => Ok((self.key(sibling), self.task_ref(sibling))),
                (None, _) => Err(format!("'{label}' is not created")),
                (Some(_), _) if self.effective_parent(sibling) == own => {
                    Ok((self.key(sibling), self.task_ref(sibling)))
                }
                (Some(_), _) => Err(format!("'{label}' does not move here")),
            };
        }
        if let Some(parent_task) = self.entries[parent].task
            && let Some(sibling) = parent_task.children.iter().find(|child| {
                child.slug == label && child.id().is_some_and(|id| !self.mentioned(id))
            })
            && let Some(id) = sibling.id()
        {
            return Ok((
                Key::Existing(id.to_string()),
                TaskRef::Existing {
                    id: id.to_string(),
                    slug: sibling.slug.clone(),
                },
            ));
        }
        Err(format!("unknown task '{label}'"))
    }

    fn mentioned(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.task.and_then(Task::id) == Some(id))
    }

    /// The key a disk edge of `task` names: its sibling on disk by slug, or a broken target.
    fn disk_target(&self, task: &Task, slug: &str) -> Key {
        let parent = task
            .id()
            .and_then(|id| self.infos.get(id))
            .and_then(|info| info.parent);
        parent
            .and_then(|parent| parent.children.iter().find(|child| child.slug == slug))
            .and_then(Task::id)
            .map_or_else(
                || Key::Broken(slug.to_string()),
                |id| Key::Existing(id.to_string()),
            )
    }

    /// The edge operations, after every task line has its place.
    fn edges(&mut self) {
        let mut candidates = Vec::new();
        for index in 0..self.entries.len() {
            if index == 0 {
                self.root_edges();
                continue;
            }
            match self.fates[index].spot {
                Spot::Here => self.reconcile_edges(index, &mut candidates),
                Spot::Stayed => self.stayed_edges(index),
                Spot::Closed | Spot::Dropped | Spot::Absent => {}
            }
        }

        let mut groups: BTreeMap<Key, Vec<Candidate>> = BTreeMap::new();
        for candidate in candidates {
            groups
                .entry(candidate.group.clone())
                .or_default()
                .push(candidate);
        }
        let splits: Vec<(usize, usize)> = {
            let mut splits: Vec<_> = self
                .keeper_of
                .iter()
                .map(|(&leaf, &keeper)| (leaf, keeper))
                .collect();
            splits.sort();
            splits
        };
        for (leaf, _) in &splits {
            groups.entry(self.key(*leaf)).or_default();
        }

        for (group, candidates) in groups {
            let mut graph = self.base_graph(&group);
            for candidate in candidates {
                let (from, target) = (&candidate.from.0, &candidate.target.0);
                if let Some(path) = closes_cycle(&graph, from, target) {
                    let names: Vec<String> = path.iter().map(|key| self.key_name(key)).collect();
                    self.edge_ops.push(Op::Kept {
                        id: candidate.anchor.clone(),
                        line: candidate.line,
                        what: format!("edge '{}' not added", candidate.text),
                        reason: format!("it would close a cycle: {}", names.join(" -> ")),
                    });
                    continue;
                }
                if candidate.kind == EdgeKind::StackedOn
                    && let Some(name) = [&candidate.from, &candidate.target]
                        .into_iter()
                        .find(|(key, _)| !self.stackable(key))
                        .map(|(_, task)| task.name().to_string())
                {
                    self.edge_ops.push(Op::Kept {
                        id: candidate.anchor.clone(),
                        line: candidate.line,
                        what: format!("edge '{}' not added", candidate.text),
                        reason: format!("{name} needs a produces-branch todo and a ships todo"),
                    });
                    continue;
                }
                graph.entry(from.clone()).or_default().push(target.clone());
                self.edge_ops.push(Op::AddEdge {
                    line: candidate.line,
                    from: candidate.from.1,
                    target: candidate.target.1,
                    kind: candidate.kind,
                    unblock: candidate.unblock,
                    default: false,
                });
            }
            if let Some(&(leaf, keeper)) = splits.iter().find(|(leaf, _)| self.key(*leaf) == group)
            {
                self.default_after_keeper(leaf, keeper, &mut graph);
            }
        }
    }

    /// D8: a fresh child of a split with no edge to the keeper gains `after <keeper>`, unless that
    /// closes a cycle.
    fn default_after_keeper(
        &mut self,
        leaf: usize,
        keeper: usize,
        graph: &mut HashMap<Key, Vec<Key>>,
    ) {
        if self.fates[keeper].spot != Spot::Here {
            return;
        }
        let keeper_label = self.entries[keeper].node.label.clone();
        let keeper_key = self.key(keeper);
        let fresh: Vec<usize> = self
            .children_of(leaf)
            .filter(|&child| {
                child != keeper
                    && self.entries[child].task.is_none()
                    && self.fates[child].spot == Spot::Here
            })
            .collect();
        for child in fresh {
            let node = self.entries[child].node;
            let names_keeper = node
                .edges
                .iter()
                .any(|edge| edge.target.len() == 1 && edge.target.first() == Some(&keeper_label));
            let key = self.key(child);
            if names_keeper || closes_cycle(graph, &key, &keeper_key).is_some() {
                continue;
            }
            graph.entry(key).or_default().push(keeper_key.clone());
            self.edge_ops.push(Op::AddEdge {
                line: node.line,
                from: self.task_ref(child),
                target: self.task_ref(keeper),
                kind: EdgeKind::After,
                unblock: default_unblock(EdgeKind::After),
                default: true,
            });
        }
    }

    /// The edges that stay in the sibling set under `group` before any edge is added: those of
    /// every task there on disk that keeps them (not moved in), less the accepted removals and
    /// the edges to tasks that move away.
    fn base_graph(&self, group: &Key) -> HashMap<Key, Vec<Key>> {
        let mut graph: HashMap<Key, Vec<Key>> = HashMap::new();
        let Key::Existing(parent_id) = group else {
            return graph;
        };
        let Some(parent) = self.infos.get(parent_id.as_str()).map(|info| info.task) else {
            return graph;
        };
        for child in &parent.children {
            let (Some(id), Some(file)) = (child.id(), &child.file) else {
                continue;
            };
            if self.moved.contains(id) {
                continue;
            }
            let from = Key::Existing(id.to_string());
            for (kind, slug, _) in disk_edges(file) {
                let target = self.disk_target(child, &slug);
                if let Key::Existing(target_id) = &target
                    && self.moved.contains(target_id.as_str())
                {
                    continue;
                }
                if self
                    .removed_edges
                    .contains(&(from.clone(), kind, target.clone()))
                {
                    continue;
                }
                graph.entry(from.clone()).or_default().push(target);
            }
        }
        graph
    }

    fn key_name(&self, key: &Key) -> String {
        match key {
            Key::Existing(id) => self
                .entries
                .iter()
                .find(|entry| entry.task.and_then(Task::id) == Some(id.as_str()))
                .map(|entry| entry.node.label.clone())
                .or_else(|| {
                    self.infos
                        .get(id.as_str())
                        .map(|info| info.task.slug.clone())
                })
                .unwrap_or_else(|| id.clone()),
            Key::New(line) => self
                .entries
                .iter()
                .find(|entry| entry.task.is_none() && entry.node.line == *line)
                .map(|entry| entry.node.label.clone())
                .unwrap_or_default(),
            Key::Broken(slug) => slug.clone(),
        }
    }

    fn stackable(&self, key: &Key) -> bool {
        let todos = match self.final_todos.get(key) {
            Some(todos) => todos.as_slice(),
            None => match key {
                Key::Existing(id) => self
                    .infos
                    .get(id.as_str())
                    .and_then(|info| info.task.file.as_ref())
                    .map_or(&[][..], |file| file.todos.as_slice()),
                Key::New(_) | Key::Broken(_) => &[][..],
            },
        };
        let has = |role| todos.iter().any(|todo| todo.role == Some(role));
        has(TodoRole::ProducesBranch) && has(TodoRole::Ships)
    }

    /// The root's edges join tasks outside the proposal, so they are never changed.
    fn root_edges(&mut self) {
        let Some(task) = self.entries[0].task else {
            return;
        };
        let Some(file) = &task.file else { return };
        let node = self.entries[0].node;
        let mut proposed: Vec<(bool, String, String)> = node
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.kind == EdgeKind::StackedOn,
                    edge.target.join("/"),
                    format!(
                        "{:?}",
                        edge.unblock.unwrap_or_else(|| default_unblock(edge.kind))
                    ),
                )
            })
            .collect();
        let mut disk: Vec<(bool, String, String)> = disk_edges(file)
            .into_iter()
            .map(|(kind, slug, unblock)| {
                (kind == EdgeKind::StackedOn, slug, format!("{unblock:?}"))
            })
            .collect();
        proposed.sort();
        disk.sort();
        if proposed != disk {
            let line = node.edges.first().map_or(node.line, |edge| edge.line);
            let id = task.id().unwrap_or_default().to_string();
            self.edge_ops.push(Op::Kept {
                id,
                line,
                what: "edges not changed".to_string(),
                reason: "the root's edges join tasks outside this proposal".to_string(),
            });
        }
    }

    /// A task whose move is refused stays with its edges on disk; edges the proposal gives it
    /// among its new siblings are kept.
    fn stayed_edges(&mut self, index: usize) {
        let entry = self.entries[index];
        let Some(task) = entry.task else { return };
        let Some(file) = &task.file else { return };
        let id = task.id().unwrap_or_default().to_string();
        let disk = disk_edges(file);
        for edge in &entry.node.edges {
            let label = edge.target.join("/");
            let unblock = edge.unblock.unwrap_or_else(|| default_unblock(edge.kind));
            let on_disk = disk.iter().any(|(kind, slug, point)| {
                *kind == edge.kind && *slug == label && *point == unblock
            });
            if !on_disk {
                self.edge_ops.push(Op::Kept {
                    id: id.clone(),
                    line: edge.line,
                    what: format!(
                        "edge '{}' not added",
                        edge_text(&entry.node.label, edge.kind, &label)
                    ),
                    reason: format!("'{}' is not moved", task.slug),
                });
            }
        }
    }

    /// The edges of a task line at its proposal place: removals and changes now, additions as
    /// candidates for the cycle and stack checks. An edge on disk that the line does not list is
    /// removed, unless the task is live (the proposal is a printed tree, so a missing edge is a
    /// deleted one); unlike the planned result and the todos, a line cannot leave edges out for
    /// brevity.
    fn reconcile_edges(&mut self, index: usize, candidates: &mut Vec<Candidate>) {
        let entry = self.entries[index];
        let node = entry.node;
        let from_key = self.key(index);
        let from_ref = self.task_ref(index);
        let Some(group) = self.effective_parent(index) else {
            return;
        };
        let anchor = self.anchor_id(index);
        let existing = entry
            .task
            .filter(|task| task.id().is_some_and(|id| !self.moved.contains(id)));
        let live = existing.and_then(|task| self.live_reason(task));

        struct Proposed {
            line: usize,
            kind: EdgeKind,
            key: Key,
            target: TaskRef,
            unblock: UnblockPoint,
            text: String,
            matched: bool,
        }
        let mut proposed = Vec::new();
        for edge in &node.edges {
            let label = edge.target.join("/");
            let text = edge_text(&node.label, edge.kind, &label);
            let resolved = match edge.target.as_slice() {
                [single] => self.resolve(index, single),
                _ => Err(format!("cross-level: '{text}' joins siblings only")),
            };
            match resolved {
                Ok((key, target)) => proposed.push(Proposed {
                    line: edge.line,
                    kind: edge.kind,
                    key,
                    target,
                    unblock: edge.unblock.unwrap_or_else(|| default_unblock(edge.kind)),
                    text,
                    matched: false,
                }),
                Err(reason) => self.edge_ops.push(Op::Kept {
                    id: anchor.clone(),
                    line: edge.line,
                    what: format!("edge '{text}' not added"),
                    reason,
                }),
            }
        }

        if let Some(task) = existing
            && let (Some(file), Some(id)) = (&task.file, task.id())
        {
            for (kind, slug, unblock) in disk_edges(file) {
                let target = self.disk_target(task, &slug);
                if let Key::Existing(target_id) = &target
                    && self.moved.contains(target_id.as_str())
                {
                    // The move of the target removes this edge.
                    continue;
                }
                let text = edge_text(&task.slug, kind, &slug);
                if let Key::Existing(target_id) = &target
                    && self.wants_move.contains(target_id.as_str())
                {
                    // The target was to move away, which would remove this edge, but its move is
                    // refused: the edge stays with it.
                    let target_slug = slug.clone();
                    if let Some(edge) = proposed
                        .iter_mut()
                        .find(|edge| !edge.matched && edge.kind == kind && edge.key == target)
                    {
                        edge.matched = true;
                    }
                    self.edge_ops.push(Op::Kept {
                        id: id.to_string(),
                        line: node.line,
                        what: format!("edge '{text}' not removed"),
                        reason: format!("'{target_slug}' is not moved"),
                    });
                    continue;
                }
                let found = proposed
                    .iter_mut()
                    .find(|edge| !edge.matched && edge.kind == kind && edge.key == target);
                match found {
                    Some(edge) if edge.unblock == unblock => edge.matched = true,
                    Some(edge) => {
                        edge.matched = true;
                        match &live {
                            Some(reason) => self.edge_ops.push(Op::Kept {
                                id: id.to_string(),
                                line: edge.line,
                                what: format!("edge '{text}' not changed"),
                                reason: reason.clone(),
                            }),
                            None => {
                                self.removed_edges
                                    .insert((from_key.clone(), kind, target.clone()));
                                self.edge_ops.push(Op::RemoveEdge {
                                    line: edge.line,
                                    id: id.to_string(),
                                    target: slug.clone(),
                                    kind,
                                    unblock,
                                });
                                edge.matched = false;
                            }
                        }
                    }
                    None => match &live {
                        Some(reason) => self.edge_ops.push(Op::Kept {
                            id: id.to_string(),
                            line: node.line,
                            what: format!("edge '{text}' not removed"),
                            reason: reason.clone(),
                        }),
                        None => {
                            self.removed_edges
                                .insert((from_key.clone(), kind, target.clone()));
                            self.edge_ops.push(Op::RemoveEdge {
                                line: node.line,
                                id: id.to_string(),
                                target: slug.clone(),
                                kind,
                                unblock,
                            });
                        }
                    },
                }
            }
        }

        for edge in proposed.into_iter().filter(|edge| !edge.matched) {
            if edge.kind == EdgeKind::StackedOn
                && let Some(reason) = &live
            {
                self.edge_ops.push(Op::Kept {
                    id: anchor.clone(),
                    line: edge.line,
                    what: format!("edge '{}' not added", edge.text),
                    reason: reason.clone(),
                });
                continue;
            }
            candidates.push(Candidate {
                line: edge.line,
                group: group.clone(),
                from: (from_key.clone(), from_ref.clone()),
                target: (edge.key, edge.target),
                kind: edge.kind,
                unblock: edge.unblock,
                text: edge.text,
                anchor: anchor.clone(),
            });
        }
    }
}

/// The edges a task file stores: its `after` edges, then its `stacked on`.
fn disk_edges(file: &bongos_work_model::TaskFile) -> Vec<(EdgeKind, String, UnblockPoint)> {
    file.after
        .iter()
        .map(|edge| (EdgeKind::After, edge.task.clone(), edge.unblock))
        .chain(
            file.stacked_on
                .iter()
                .map(|edge| (EdgeKind::StackedOn, edge.task.clone(), edge.unblock)),
        )
        .collect()
}

fn edge_text(from: &str, kind: EdgeKind, target: &str) -> String {
    let word = match kind {
        EdgeKind::After => "after",
        EdgeKind::StackedOn => "stacked on",
    };
    format!("{from} {word} {target}")
}

/// The chain a new edge `from -> target` closes over `graph`, from the target back to it, or
/// `None`.
fn closes_cycle(graph: &HashMap<Key, Vec<Key>>, from: &Key, target: &Key) -> Option<Vec<Key>> {
    fn walk(
        node: &Key,
        goal: &Key,
        graph: &HashMap<Key, Vec<Key>>,
        path: &mut Vec<Key>,
        seen: &mut HashSet<Key>,
    ) -> bool {
        path.push(node.clone());
        if node == goal {
            return true;
        }
        if seen.insert(node.clone()) {
            for next in graph.get(node).into_iter().flatten() {
                if walk(next, goal, graph, path, seen) {
                    return true;
                }
            }
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    if !walk(target, from, graph, &mut path, &mut HashSet::new()) {
        return None;
    }
    path.push(target.clone());
    Some(path)
}

/// Text as the printer writes it: one line, trimmed.
fn normal(text: &str) -> String {
    text.split(['\r', '\n'])
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn same(a: &str, b: &str) -> bool {
    normal(a) == normal(b)
}

fn same_opt(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn open_todo(name: &str, role: Option<TodoRole>, workflow: Option<String>) -> Todo {
    Todo {
        name: name.to_string(),
        role,
        workflow,
        state: TodoState::Open,
        started: None,
        extra: Extra::new(),
    }
}

/// The proposal's todo lines with D3's creation rule: on a PR kind, when no line has a role,
/// `produces-branch` goes on `implement` and `ships` on `ship` or `merge`, once each.
///
/// A line that lines up with a todo already on `disk` gets the role only when that todo carries
/// it: a leaf whose `implement` deliberately has no role stays so, while re-applying a proposal
/// that created the roles diffs to nothing.
fn with_creation_roles(lines: &[grammar::Todo], kind: Kind, disk: &[Todo]) -> Vec<grammar::Todo> {
    let mut lines = lines.to_vec();
    if matches!(kind, Kind::PrShip | Kind::PrMerge) && lines.iter().all(|line| line.role.is_none())
    {
        for (names, role) in [
            (&["implement"][..], TodoRole::ProducesBranch),
            (&["ship", "merge"][..], TodoRole::Ships),
        ] {
            if let Some(line) = lines
                .iter_mut()
                .find(|line| names.contains(&line.name.as_str()))
            {
                let on_disk = disk.iter().find(|todo| same(&todo.name, &line.name));
                if on_disk.is_none_or(|todo| todo.role == Some(role)) {
                    line.role = Some(role);
                }
            }
        }
    }
    lines
}

/// The todos a new leaf starts with: its lines (with the creation roles), else its kind's.
fn new_todos(lines: &[grammar::Todo], kind: Kind) -> Vec<Todo> {
    if lines.is_empty() {
        return default_todos(kind);
    }
    with_creation_roles(lines, kind, &[])
        .into_iter()
        .map(|line| open_todo(&line.name, line.role, line.workflow))
        .collect()
}

fn split_work(task: &Task) -> SplitWork {
    let count = |dir: &str| {
        task.files
            .keys()
            .filter(|path| path.starts_with(dir))
            .filter_map(|path| path.components().nth(1))
            .map(|component| component.as_os_str().to_owned())
            .collect::<HashSet<_>>()
            .len()
    };
    let file = task.file.as_ref();
    SplitWork {
        todos: file.map(|file| file.todos.clone()).unwrap_or_default(),
        branch: file.and_then(|file| file.branch.clone()),
        worktree: file.and_then(|file| file.worktree.clone()),
        result: file.is_some_and(|file| file.result.is_some()),
        planned_result: file.is_some_and(|file| file.planned_result.is_some()),
        chats: count(CHATS_DIR),
        runs: count(RUNS_DIR),
    }
}

/// Why the root's open `plans` todo, which this proposal completes, is frozen.
const PLAN_TODO: &str = "the plan todo";

/// A reconciled todo list (D6): the new list, the changes, and what is kept as `(line, what,
/// reason)`.
struct Reconciled {
    todos: Vec<Todo>,
    ops: Vec<TodoOp>,
    kept: Vec<(usize, String, String)>,
}

/// One todo of the new list that is not frozen.
struct Placed {
    /// Its position among the proposal's lines.
    position: usize,
    slot: usize,
    todo: Todo,
    /// Its index on disk, for an existing todo.
    existing: Option<usize>,
    moved: bool,
    after_frozen: Option<String>,
}

/// Lays the proposal's todo `lines` around the frozen todos of `disk` (D6). Frozen todos keep
/// their exact content and their order; a line naming one is matched by name, and a line that
/// would edit, move or drop it is kept with the reason. The other lines are aligned with the open
/// todos by the longest common subsequence of names: aligned pairs keep their todo (a role or
/// workflow change is a set), unaligned pairs at the same place are renames, and the rest are adds
/// and removes (an add and a remove of one name are a move). A new todo placed before a frozen one
/// goes after the last frozen one, and an open todo never moves before a frozen one it was after.
fn reconcile(
    disk: &[Todo],
    frozen: &[Option<&'static str>],
    lines: &[grammar::Todo],
    node_line: usize,
) -> Reconciled {
    let reason_of = |index: usize| frozen.get(index).copied().flatten();
    let frozen_indexes: Vec<usize> = (0..disk.len())
        .filter(|&index| reason_of(index).is_some())
        .collect();
    let mut consumed = vec![false; lines.len()];
    let mut kept = Vec::new();
    // For each frozen todo, the position of the line matched to it in order.
    let mut matched_at: Vec<Option<usize>> = vec![None; frozen_indexes.len()];
    let mut unmatched = Vec::new();
    let mut last: Option<usize> = None;
    for (slot, &index) in frozen_indexes.iter().enumerate() {
        let todo = &disk[index];
        let reason = reason_of(index).unwrap_or_default();
        let start = last.map_or(0, |position| position + 1);
        let named =
            |position: &usize| !consumed[*position] && same(&lines[*position].name, &todo.name);
        if let Some(position) = (start..lines.len()).find(named) {
            consumed[position] = true;
            matched_at[slot] = Some(position);
            last = Some(position);
            let line = &lines[position];
            if line.role != todo.role
                || !same_opt(line.workflow.as_deref(), todo.workflow.as_deref())
            {
                kept.push((
                    line.line,
                    format!("todo '{}' not changed", todo.name),
                    reason.to_string(),
                ));
            }
        } else if let Some(position) = (0..start).find(named) {
            consumed[position] = true;
            kept.push((
                lines[position].line,
                format!("todo '{}' not moved", todo.name),
                reason.to_string(),
            ));
        } else {
            unmatched.push(index);
        }
    }
    // A line printed as closed or started that names no todo here is a frozen todo renamed.
    let open_names: Vec<&str> = (0..disk.len())
        .filter(|&index| reason_of(index).is_none())
        .map(|index| disk[index].name.as_str())
        .collect();
    let mut renamed_frozen = (0..lines.len()).filter(|&position| {
        let line = &lines[position];
        !consumed[position]
            && (line.mark.is_some() || line.started)
            && !open_names.iter().any(|name| same(name, &line.name))
    });
    let mut pairs = Vec::new();
    for index in unmatched {
        let todo = &disk[index];
        let reason = reason_of(index).unwrap_or_default().to_string();
        match renamed_frozen.next() {
            Some(position) => pairs.push((position, index, reason)),
            // The plan todo may be left out: the one this proposal completes, and one done by an
            // earlier proposal.
            None if reason == PLAN_TODO || todo.role == Some(TodoRole::Plans) => {}
            None => kept.push((
                node_line,
                format!("todo '{}' not removed", todo.name),
                reason,
            )),
        }
    }
    for (position, index, reason) in pairs {
        consumed[position] = true;
        kept.push((
            lines[position].line,
            format!(
                "todo '{}' not renamed to '{}'",
                disk[index].name, lines[position].name
            ),
            reason,
        ));
    }

    // The rest: the free lines against the open todos.
    let free: Vec<usize> = (0..lines.len())
        .filter(|&position| !consumed[position])
        .collect();
    let open: Vec<usize> = (0..disk.len())
        .filter(|&index| reason_of(index).is_none())
        .collect();
    let anchors = lcs(
        &free
            .iter()
            .map(|&position| normal(&lines[position].name))
            .collect::<Vec<_>>(),
        &open
            .iter()
            .map(|&index| normal(&disk[index].name))
            .collect::<Vec<_>>(),
    );
    let mut assigned: Vec<Option<(usize, bool)>> = vec![None; free.len()];
    let mut removed: Vec<usize> = Vec::new();
    let mut previous = (0, 0);
    for (a, b) in anchors
        .iter()
        .copied()
        .chain(std::iter::once((free.len(), open.len())))
    {
        let (free_gap, open_gap) = (previous.0..a, previous.1..b);
        let pair = free_gap.len().min(open_gap.len());
        for offset in 0..pair {
            assigned[previous.0 + offset] = Some((open[previous.1 + offset], false));
        }
        removed.extend(open_gap.clone().skip(pair).map(|at| open[at]));
        if a < free.len() {
            assigned[a] = Some((open[b], false));
        }
        previous = (a + 1, b + 1);
    }
    // An add and a remove of one name are a move.
    for (at, &position) in free.iter().enumerate() {
        if assigned[at].is_some() {
            continue;
        }
        if let Some(found) = removed
            .iter()
            .position(|&index| same(&disk[index].name, &lines[position].name))
        {
            assigned[at] = Some((removed.remove(found), true));
        }
    }

    let frozen_count = frozen_indexes.len();
    let last_frozen = frozen_indexes.last().map(|&index| disk[index].name.clone());
    let frozen_before_position = |position: usize| {
        matched_at
            .iter()
            .filter(|at| at.is_some_and(|at| at < position))
            .count()
    };
    let frozen_before_index =
        |index: usize| frozen_indexes.iter().filter(|&&at| at < index).count();
    let frozen_roles: Vec<(TodoRole, usize)> = frozen_indexes
        .iter()
        .filter_map(|&index| Some((disk[index].role?, index)))
        .collect();

    let mut placed = Vec::new();
    for (at, &position) in free.iter().enumerate() {
        let line = &lines[position];
        let wanted = frozen_before_position(position);
        let mut role = line.role;
        if let Some(wanted_role) = role
            && let Some(&(_, holder)) = frozen_roles.iter().find(|(held, _)| *held == wanted_role)
        {
            kept.push((
                line.line,
                format!(
                    "role {} on todo '{}' not set",
                    role_name(wanted_role),
                    line.name
                ),
                format!(
                    "todo '{}' has it and is {}",
                    disk[holder].name,
                    reason_of(holder).unwrap_or_default()
                ),
            ));
            role = assigned[at]
                .and_then(|(index, _)| disk[index].role)
                .filter(|role| *role != wanted_role);
        }
        match assigned[at] {
            Some((index, moved)) => {
                let before = frozen_before_index(index);
                let slot = wanted.max(before);
                let mut todo = disk[index].clone();
                todo.name = line.name.clone();
                todo.role = role;
                todo.workflow = line.workflow.clone();
                placed.push(Placed {
                    position,
                    slot,
                    todo,
                    existing: Some(index),
                    moved: moved || slot != before,
                    after_frozen: None,
                });
            }
            None => {
                // Said only when the proposal lists a frozen todo after this line.
                let before_frozen = matched_at
                    .iter()
                    .any(|at| at.is_some_and(|at| at > position));
                let after_frozen = before_frozen.then(|| last_frozen.clone()).flatten();
                placed.push(Placed {
                    position,
                    slot: frozen_count,
                    todo: open_todo(&line.name, role, line.workflow.clone()),
                    existing: None,
                    moved: false,
                    after_frozen,
                });
            }
        }
    }
    placed.sort_by_key(|item| (item.slot, item.position));

    let mut todos = Vec::new();
    let mut items = placed.into_iter().peekable();
    for slot in 0..=frozen_count {
        while let Some(item) = items.next_if(|item| item.slot == slot) {
            todos.push((Some(item), None));
        }
        if let Some(&index) = frozen_indexes.get(slot) {
            todos.push((None, Some(index)));
        }
    }

    let mut ops = Vec::new();
    for (new_index, (item, _)) in todos.iter().enumerate() {
        let Some(item) = item else { continue };
        match item.existing {
            None => ops.push(TodoOp::Add {
                name: item.todo.name.clone(),
                index: new_index,
                after_frozen: item.after_frozen.clone(),
            }),
            Some(index) => {
                let old = &disk[index];
                if !same(&old.name, &item.todo.name) {
                    ops.push(TodoOp::Rename {
                        index,
                        from: old.name.clone(),
                        to: item.todo.name.clone(),
                    });
                }
                if old.role != item.todo.role {
                    ops.push(TodoOp::SetRole {
                        name: item.todo.name.clone(),
                        role: item.todo.role,
                    });
                }
                if !same_opt(old.workflow.as_deref(), item.todo.workflow.as_deref()) {
                    ops.push(TodoOp::SetWorkflow {
                        name: item.todo.name.clone(),
                        workflow: item.todo.workflow.clone(),
                    });
                }
                if item.moved {
                    ops.push(TodoOp::Move {
                        name: item.todo.name.clone(),
                        from: index,
                        to: new_index,
                    });
                }
            }
        }
    }
    removed.sort();
    for index in removed {
        ops.push(TodoOp::Remove {
            index,
            name: disk[index].name.clone(),
        });
    }
    let todos = todos
        .into_iter()
        .filter_map(|(item, frozen)| match (item, frozen) {
            (Some(item), _) => Some(item.todo),
            (None, Some(index)) => disk.get(index).cloned(),
            (None, None) => None,
        })
        .collect();
    Reconciled { todos, ops, kept }
}

/// The index pairs of a longest common subsequence of `a` and `b`, in order.
fn lcs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut pairs = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}
