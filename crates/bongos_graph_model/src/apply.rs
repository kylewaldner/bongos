//! Applying a batch of graph edits to a project's parsed definitions (D3). [`apply`] is pure: it
//! reads the tree, the run's context and the live facts it is handed, and returns the definition
//! changes to land, or the first reason the batch is refused. No git, store, process or clock.
//!
//! Ops apply in order to an in-memory copy of the tree, so a later op sees what an earlier one
//! did. The resulting [`DefinitionOp`]s only write `tasks/**/task.toml` files, move `tasks/**`
//! folders and delete `tasks/**/task.toml` files: the top group's `task.toml`, `project.toml`
//! and every other folder of `definitions/` are never touched.
//!
//! A landing makes the moves first, each against the tree the earlier ones left, then the deletes
//! and writes, which name the paths after the moves. So the copy tracks where every task file of
//! the original tree sits after the moves so far (including the files of removed tasks, deleted
//! last), and a folder still holding one is never reused for a new or moved task.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use bongos_project_repo_model::layout::{DOCUMENTS_DIR, TASK_FILE, TASKS_DIR};
use bongos_project_repo_model::moves::child_folder;
use bongos_project_repo_model::{
    DefinitionNode, DefinitionOp, DefinitionTree, RelPath, TaskDefinition, TaskStateRecord,
    TodoDefinition, join,
};
use bongos_work_model::commands::{default_todos, default_unblock, free_slug};
use bongos_work_model::rules::{self, Place};
use bongos_work_model::slug::slugify;
use bongos_work_model::store::Task;
use bongos_work_model::{Edge, Extra, Kind, SCHEMA, StackedOn, TaskId, Timestamp};
use bongos_workflow_model::definition::Scope;

use crate::ops::{BatchError, EdgeKind, GraphEditBatch, GraphOp, NewTodo, TaskRef, check_batch};
use crate::scope;

/// What the app knows about tasks besides their definitions. In the app it is backed by the
/// project's live facts (state rows, runs, claims, moves in flight).
pub trait StartedFacts {
    /// Whether the task has work: a todo with state, a branch, a worktree, a result or an
    /// active run.
    fn started(&self, task: &str) -> bool;
    /// Why the task is held (a move in flight), if it is.
    fn held(&self, task: &str) -> Option<String>;
    /// Whether the task's runs folder is not empty.
    fn has_runs(&self, task: &str) -> bool;
}

/// The run a batch comes from.
pub struct ApplyCtx<'a> {
    /// The project the tree is; a batch naming another project is outside the scope.
    pub project_id: String,
    /// The task the run is on.
    pub run_task: TaskId,
    pub scope: Scope,
    pub live: &'a dyn StartedFacts,
    /// A new UUID v7 task or todo id.
    pub new_id: &'a dyn Fn() -> TaskId,
    /// The refs the run's earlier batches created, and on a rebuild the ids this batch's creates
    /// got the first time.
    pub refs: &'a BTreeMap<String, TaskId>,
}

/// What a batch does.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    /// Moves first, then deletes, then writes (each sorted by path).
    pub ops: Vec<DefinitionOp>,
    /// The ref and id of each create that has a ref, in op order.
    pub created: Vec<(String, TaskId)>,
    /// Every task created, changed, moved (with its subtree) or removed (with its subtree).
    pub touched: BTreeSet<TaskId>,
}

/// What a started task cannot have done to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartedChange {
    Retagged,
    Resized,
    NewWorkKind,
    Reordered,
    Moved,
    Removed,
}

impl StartedChange {
    fn words(self) -> &'static str {
        match self {
            StartedChange::Retagged => "retagged",
            StartedChange::Resized => "resized",
            StartedChange::NewWorkKind => "given a new work kind",
            StartedChange::Reordered => "reordered",
            StartedChange::Moved => "moved",
            StartedChange::Removed => "removed",
        }
    }
}

/// Why a batch is refused; the whole batch is, never part of it.
#[derive(Clone, Debug, PartialEq)]
pub enum EditRefusal {
    /// The workflow's scope is `none`.
    NotDeclared,
    /// The batch is malformed (see [`check_batch`]).
    Invalid(BatchError),
    /// `task` (an id, or `project <name>` for a batch naming another project) is outside the
    /// scope.
    OutsideScope {
        task: String,
        scope: Scope,
        run_task: TaskId,
        project: String,
    },
    /// The id or `@ref` as the op wrote it.
    UnknownTask(String),
    /// By slug.
    SiblingsOnly {
        a: String,
        b: String,
    },
    /// The slugs of the chain the new edge closes, `[a, b, a]`.
    Cycle(Vec<String>),
    Started {
        goal: String,
        change: StartedChange,
    },
    HasRuns {
        goal: String,
    },
    Held {
        goal: String,
        reason: String,
    },
    TopGroup,
    IntoOwnSubtree {
        goal: String,
    },
    /// A create's id (a recorded ref's) is already a task of the tree.
    DuplicateId(TaskId),
    SelfEdge(String),
    DuplicateEdge {
        a: String,
        b: String,
    },
    AlreadyStacked {
        a: String,
        on: String,
    },
    NoEdge {
        a: String,
        b: String,
        kind: EdgeKind,
    },
    /// A definition that could not be written; never expected.
    Internal(String),
}

impl fmt::Display for EditRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditRefusal::NotDeclared => f.write_str("graph edits not declared"),
            EditRefusal::Invalid(error) => write!(f, "invalid graph edits: {error}"),
            EditRefusal::OutsideScope {
                task,
                scope,
                run_task,
                project,
            } => match scope {
                Scope::None => f.write_str("graph edits not declared"),
                Scope::OwnSubtree => write!(
                    f,
                    "{task} is outside the declared scope (own subtree of {run_task})"
                ),
                Scope::OwnProject => write!(
                    f,
                    "{task} is outside the declared scope (own project {project})"
                ),
                Scope::Projects => write!(
                    f,
                    "{task} is outside the declared scope (projects, editing {project})"
                ),
            },
            EditRefusal::UnknownTask(task) => write!(f, "unknown task {task}"),
            EditRefusal::SiblingsOnly { a, b } => write!(f, "edges join siblings only: {a} and {b}"),
            EditRefusal::Cycle(path) => write!(f, "cycle: {}", path.join(" -> ")),
            EditRefusal::Started { goal, change } => {
                write!(f, "started task `{goal}` cannot be {}", change.words())
            }
            EditRefusal::HasRuns { goal } => write!(f, "task `{goal}` has runs"),
            EditRefusal::Held { goal, reason } => write!(f, "task `{goal}` is held: {reason}"),
            EditRefusal::TopGroup => f.write_str("the top group cannot be edited"),
            EditRefusal::IntoOwnSubtree { goal } => {
                write!(f, "task `{goal}` cannot be moved into its own subtree")
            }
            EditRefusal::DuplicateId(id) => write!(f, "task {id} already exists"),
            EditRefusal::SelfEdge(a) => write!(f, "{a} cannot have an edge to itself"),
            EditRefusal::DuplicateEdge { a, b } => write!(f, "{a} already has an edge to {b}"),
            EditRefusal::AlreadyStacked { a, on } => write!(
                f,
                "{a} is already stacked on {on}; a task has at most one stacked on"
            ),
            EditRefusal::NoEdge { a, b, kind } => {
                write!(f, "{a} has no {} edge to {b}", kind.as_str())
            }
            EditRefusal::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EditRefusal {}

/// Applies `batch` to `tree` (master's definitions of `ctx.project_id`).
pub fn apply(
    tree: &DefinitionTree,
    batch: &GraphEditBatch,
    ctx: &ApplyCtx,
) -> Result<Applied, EditRefusal> {
    if ctx.scope == Scope::None {
        return Err(EditRefusal::NotDeclared);
    }
    check_batch(batch, ctx.refs).map_err(EditRefusal::Invalid)?;
    if let Some(project) = &batch.project
        && !scope::project_allowed(ctx, project)
    {
        return Err(EditRefusal::OutsideScope {
            task: format!("project {project}"),
            scope: ctx.scope,
            run_task: ctx.run_task.clone(),
            project: ctx.project_id.clone(),
        });
    }
    let mut work = Work::new(tree, ctx);
    for op in &batch.ops {
        work.op(op)?;
    }
    work.finish()
}

/// Where a task file of the original tree sits after the moves so far.
struct IndexEntry {
    id: TaskId,
    folder: String,
    removed: bool,
}

struct Work<'c, 'a> {
    ctx: &'c ApplyCtx<'a>,
    tree: DefinitionTree,
    original: BTreeMap<TaskId, TaskDefinition>,
    index: Vec<IndexEntry>,
    moves: Vec<(String, String)>,
    refs: BTreeMap<String, TaskId>,
    created: Vec<(String, TaskId)>,
    touched: BTreeSet<TaskId>,
}

impl<'c, 'a> Work<'c, 'a> {
    fn new(tree: &DefinitionTree, ctx: &'c ApplyCtx<'a>) -> Self {
        let nodes = tree.nodes();
        Self {
            ctx,
            tree: tree.clone(),
            original: nodes
                .iter()
                .map(|node| (node.definition.id.clone(), node.definition.clone()))
                .collect(),
            index: nodes
                .iter()
                .map(|node| IndexEntry {
                    id: node.definition.id.clone(),
                    folder: node.folder.clone(),
                    removed: false,
                })
                .collect(),
            moves: Vec::new(),
            refs: ctx.refs.clone(),
            created: Vec::new(),
            touched: BTreeSet::new(),
        }
    }

    fn op(&mut self, op: &GraphOp) -> Result<(), EditRefusal> {
        match op {
            GraphOp::Create {
                reference,
                parent,
                goal,
                work_kind,
                areas,
                size,
                repositories,
                todos,
            } => self.create(
                reference.as_deref(),
                parent,
                goal,
                work_kind,
                areas,
                *size,
                repositories,
                todos,
            ),
            GraphOp::Set {
                task,
                goal,
                work_kind,
                size,
                areas,
            } => self.set(task, goal.as_deref(), work_kind, *size, areas.as_ref()),
            GraphOp::Move { task, parent } => self.move_task(task, parent),
            GraphOp::Remove { task } => self.remove(task),
            GraphOp::AddEdge {
                from,
                to,
                kind,
                unblock,
            } => self.add_edge(from, to, *kind, *unblock),
            GraphOp::RemoveEdge { from, to, kind } => self.remove_edge(from, to, *kind),
        }
    }

    fn node(&self, id: &str) -> Result<&DefinitionNode, EditRefusal> {
        self.tree
            .node(id)
            .ok_or_else(|| EditRefusal::UnknownTask(id.to_string()))
    }

    fn node_mut(&mut self, id: &str) -> Result<&mut DefinitionNode, EditRefusal> {
        self.tree
            .top
            .as_mut()
            .and_then(|top| find_mut(top, id))
            .ok_or_else(|| EditRefusal::UnknownTask(id.to_string()))
    }

    fn goal(&self, id: &str) -> String {
        self.tree
            .task(id)
            .map(|definition| definition.goal.clone())
            .unwrap_or_default()
    }

    fn resolve(&self, task: &TaskRef) -> Result<TaskId, EditRefusal> {
        let unknown = || EditRefusal::UnknownTask(task.to_string());
        let id = match task {
            TaskRef::Id(id) => id.clone(),
            TaskRef::Ref(name) => self.refs.get(name).cloned().ok_or_else(unknown)?,
        };
        if self.tree.node(&id).is_none() {
            return Err(unknown());
        }
        Ok(id)
    }

    fn in_scope(&self, id: &str) -> Result<(), EditRefusal> {
        if scope::contains(&self.tree, self.ctx, id) {
            Ok(())
        } else {
            Err(EditRefusal::OutsideScope {
                task: id.to_string(),
                scope: self.ctx.scope,
                run_task: self.ctx.run_task.clone(),
                project: self.ctx.project_id.clone(),
            })
        }
    }

    fn not_held(&self, id: &str) -> Result<(), EditRefusal> {
        match self.ctx.live.held(id) {
            Some(reason) => Err(EditRefusal::Held {
                goal: self.goal(id),
                reason,
            }),
            None => Ok(()),
        }
    }

    /// An existing task an op changes: in scope, not the top group, not held.
    fn editable(&self, id: &str) -> Result<(), EditRefusal> {
        self.in_scope(id)?;
        if self.node(id)?.folder.is_empty() {
            return Err(EditRefusal::TopGroup);
        }
        self.not_held(id)
    }

    fn require_not_started(&self, id: &str, change: StartedChange) -> Result<(), EditRefusal> {
        if self.ctx.live.started(id) {
            Err(EditRefusal::Started {
                goal: self.goal(id),
                change,
            })
        } else {
            Ok(())
        }
    }

    fn parent_id(&self, id: &str) -> Option<TaskId> {
        self.tree
            .nodes()
            .into_iter()
            .find(|node| node.children.iter().any(|child| child.definition.id == id))
            .map(|node| node.definition.id.clone())
    }

    /// The slugs a new child of `parent` may not take: its children's, the folders under it that
    /// still hold a task file of the original tree, and the reserved names.
    fn taken_slugs(&self, parent: &DefinitionNode) -> Vec<String> {
        let mut taken: Vec<String> = parent
            .children
            .iter()
            .map(|child| child.slug.clone())
            .collect();
        for entry in &self.index {
            let (entry_parent, slug) = split_folder(&entry.folder);
            if !entry.folder.is_empty() && entry_parent == parent.folder {
                taken.push(slug.to_string());
            }
        }
        taken.push(DOCUMENTS_DIR.to_string());
        taken
    }

    /// A slug for a new child of `parent`, `base` or `base-2` and on (03's rules).
    fn free_slug(&self, base: &str, parent: &DefinitionNode) -> String {
        let taken = self.taken_slugs(parent);
        let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
        free_slug(base, &taken)
    }

    fn attach(&mut self, parent: &str, child: DefinitionNode) -> Result<(), EditRefusal> {
        let parent = self.node_mut(parent)?;
        parent.children.push(child);
        parent.children.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(())
    }

    /// Records the move of a folder, when it holds a task file of the original tree.
    fn record_move(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let mut moved = false;
        for entry in &mut self.index {
            if entry.folder == from || entry.folder.starts_with(&prefix) {
                entry.folder = format!("{to}{}", &entry.folder[from.len()..]);
                moved = true;
            }
        }
        if moved {
            self.moves.push((from.to_string(), to.to_string()));
        }
    }

    /// Drops the edges the siblings of the task `slug` (under `parent`) hold to it, as it leaves
    /// them. A started or held sibling holding one refuses.
    fn drop_edges_to(&mut self, parent: &str, slug: &str) -> Result<(), EditRefusal> {
        let holders: Vec<TaskId> = self
            .node(parent)?
            .children
            .iter()
            .filter(|sibling| sibling.slug != slug && holds_edge_to(&sibling.definition, slug))
            .map(|sibling| sibling.definition.id.clone())
            .collect();
        for holder in &holders {
            self.not_held(holder)?;
            self.require_not_started(holder, StartedChange::Reordered)?;
        }
        for holder in holders {
            let definition = &mut self.node_mut(&holder)?.definition;
            definition.after.retain(|edge| edge.task != slug);
            if definition
                .stacked_on
                .as_ref()
                .is_some_and(|edge| edge.task == slug)
            {
                definition.stacked_on = None;
            }
            self.touched.insert(holder);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &mut self,
        reference: Option<&str>,
        parent: &TaskRef,
        goal: &str,
        work_kind: &Option<String>,
        areas: &[String],
        size: Option<u32>,
        repositories: &[String],
        todos: &[NewTodo],
    ) -> Result<(), EditRefusal> {
        let parent = self.resolve(parent)?;
        self.in_scope(&parent)?;
        self.not_held(&parent)?;
        let id = reference
            .and_then(|name| self.ctx.refs.get(name))
            .cloned()
            .unwrap_or_else(|| (self.ctx.new_id)());
        if self.tree.node(&id).is_some() {
            return Err(EditRefusal::DuplicateId(id));
        }
        let goal = goal.trim().to_string();
        let parent_node = self.node(&parent)?;
        let slug = self.free_slug(&slugify(&goal), parent_node);
        let folder = child_folder(&parent_node.folder, &slug);
        let todos = if todos.is_empty() {
            default_todos(Kind::PrShip)
                .into_iter()
                .map(|todo| TodoDefinition {
                    id: Some((self.ctx.new_id)()),
                    name: todo.name,
                    role: todo.role,
                    workflow: todo.workflow,
                    extra: Extra::new(),
                })
                .collect()
        } else {
            todos
                .iter()
                .map(|todo| TodoDefinition {
                    id: Some((self.ctx.new_id)()),
                    name: todo.name.clone(),
                    role: None,
                    workflow: todo.workflow.clone(),
                    extra: Extra::new(),
                })
                .collect()
        };
        let definition = TaskDefinition {
            schema: SCHEMA,
            id: id.clone(),
            goal,
            kind: Kind::PrShip,
            created: created_at(&id),
            repository: None,
            work_kind: clean_work_kind(work_kind.as_deref()),
            areas: areas.to_vec(),
            size,
            repositories: repositories.to_vec(),
            merge_is_ship: None,
            activity: None,
            after: Vec::new(),
            stacked_on: None,
            todos,
            planned_result: None,
            extra: Extra::new(),
        };
        self.attach(
            &parent,
            DefinitionNode {
                folder,
                slug,
                definition,
                children: Vec::new(),
            },
        )?;
        self.touched.insert(id.clone());
        if let Some(name) = reference {
            self.refs.insert(name.to_string(), id.clone());
            self.created.push((name.to_string(), id));
        }
        Ok(())
    }

    fn set(
        &mut self,
        task: &TaskRef,
        goal: Option<&str>,
        work_kind: &Option<String>,
        size: Option<u32>,
        areas: Option<&Vec<String>>,
    ) -> Result<(), EditRefusal> {
        let id = self.resolve(task)?;
        self.editable(&id)?;
        let current = self.node(&id)?.definition.clone();
        let mut next = current.clone();
        if let Some(areas) = areas
            && *areas != current.areas
        {
            self.require_not_started(&id, StartedChange::Retagged)?;
            next.areas = areas.clone();
        }
        if let Some(size) = size
            && current.size != Some(size)
        {
            self.require_not_started(&id, StartedChange::Resized)?;
            next.size = Some(size);
        }
        if let Some(work_kind) = work_kind {
            let work_kind = clean_work_kind(Some(work_kind));
            if work_kind != current.work_kind {
                self.require_not_started(&id, StartedChange::NewWorkKind)?;
                next.work_kind = work_kind;
            }
        }
        // Retitling a started task is allowed: the landing checks do not guard the goal.
        if let Some(goal) = goal {
            next.goal = goal.trim().to_string();
        }
        if next != current {
            self.node_mut(&id)?.definition = next;
            self.touched.insert(id);
        }
        Ok(())
    }

    /// Refuses when any task of the subtree of `id` is held, or is started.
    fn subtree_unstarted(&self, id: &str, change: StartedChange) -> Result<Vec<TaskId>, EditRefusal> {
        let mut ids = Vec::new();
        collect_ids(self.node(id)?, &mut ids);
        for task in &ids {
            self.not_held(task)?;
            self.require_not_started(task, change)?;
        }
        Ok(ids)
    }

    fn move_task(&mut self, task: &TaskRef, parent: &TaskRef) -> Result<(), EditRefusal> {
        let id = self.resolve(task)?;
        self.editable(&id)?;
        let new_parent = self.resolve(parent)?;
        self.in_scope(&new_parent)?;
        self.not_held(&new_parent)?;
        if scope::is_ancestor(&self.tree, &id, &new_parent) {
            return Err(EditRefusal::IntoOwnSubtree {
                goal: self.goal(&id),
            });
        }
        let ids = self.subtree_unstarted(&id, StartedChange::Moved)?;
        let Some(old_parent) = self.parent_id(&id) else {
            return Err(EditRefusal::TopGroup);
        };
        if old_parent == new_parent {
            return Ok(());
        }
        let old_slug = self.node(&id)?.slug.clone();
        self.drop_edges_to(&old_parent, &old_slug)?;
        let mut node = self
            .tree
            .top
            .as_mut()
            .and_then(|top| detach(top, &id))
            .ok_or_else(|| EditRefusal::UnknownTask(id.clone()))?;
        node.definition.after.clear();
        node.definition.stacked_on = None;
        let parent_node = self.node(&new_parent)?;
        let slug = self.free_slug(&node.slug, parent_node);
        let from = node.folder.clone();
        let to = child_folder(&parent_node.folder, &slug);
        refolder(&mut node, to.clone());
        self.attach(&new_parent, node)?;
        self.record_move(&from, &to);
        self.touched.extend(ids);
        Ok(())
    }

    fn remove(&mut self, task: &TaskRef) -> Result<(), EditRefusal> {
        let id = self.resolve(task)?;
        self.editable(&id)?;
        let ids = self.subtree_unstarted(&id, StartedChange::Removed)?;
        for task in &ids {
            if self.ctx.live.has_runs(task) {
                return Err(EditRefusal::HasRuns {
                    goal: self.goal(task),
                });
            }
        }
        let Some(parent) = self.parent_id(&id) else {
            return Err(EditRefusal::TopGroup);
        };
        let slug = self.node(&id)?.slug.clone();
        self.drop_edges_to(&parent, &slug)?;
        self.tree
            .top
            .as_mut()
            .and_then(|top| detach(top, &id))
            .ok_or_else(|| EditRefusal::UnknownTask(id.clone()))?;
        for entry in &mut self.index {
            if ids.contains(&entry.id) {
                entry.removed = true;
            }
        }
        self.touched.extend(ids);
        Ok(())
    }

    /// The two ends of an edge op: both in scope, distinct siblings. Returns their parent.
    fn edge_ends(&self, a: &str, b: &str) -> Result<TaskId, EditRefusal> {
        self.in_scope(a)?;
        self.in_scope(b)?;
        let slug_a = self.node(a)?.slug.clone();
        let slug_b = self.node(b)?.slug.clone();
        if a == b {
            return Err(EditRefusal::SelfEdge(slug_a));
        }
        match (self.parent_id(a), self.parent_id(b)) {
            (Some(pa), Some(pb)) if pa == pb => Ok(pa),
            _ => Err(EditRefusal::SiblingsOnly {
                a: slug_a,
                b: slug_b,
            }),
        }
    }

    fn add_edge(
        &mut self,
        from: &TaskRef,
        to: &TaskRef,
        kind: EdgeKind,
        unblock: Option<bongos_work_model::UnblockPoint>,
    ) -> Result<(), EditRefusal> {
        let a = self.resolve(from)?;
        let b = self.resolve(to)?;
        let parent = self.edge_ends(&a, &b)?;
        self.not_held(&a)?;
        self.require_not_started(&a, StartedChange::Reordered)?;
        let holder = self.node(&a)?;
        let slug_a = holder.slug.clone();
        let slug_b = self.node(&b)?.slug.clone();
        if holds_edge_to(&holder.definition, &slug_b) {
            return Err(EditRefusal::DuplicateEdge {
                a: slug_a,
                b: slug_b,
            });
        }
        if kind == EdgeKind::StackedOn
            && let Some(existing) = &holder.definition.stacked_on
        {
            return Err(EditRefusal::AlreadyStacked {
                a: slug_a,
                on: existing.task.clone(),
            });
        }
        if let Some(path) = sibling_cycle(self.node(&parent)?, &a, &slug_b) {
            return Err(EditRefusal::Cycle(path));
        }
        let unblock = unblock.unwrap_or_else(|| default_unblock(kind.to_rules()));
        let definition = &mut self.node_mut(&a)?.definition;
        match kind {
            EdgeKind::After => definition.after.push(Edge {
                task: slug_b,
                unblock,
                extra: Extra::new(),
            }),
            EdgeKind::StackedOn => {
                definition.stacked_on = Some(StackedOn {
                    task: slug_b,
                    unblock,
                    extra: Extra::new(),
                })
            }
        }
        self.touched.insert(a);
        Ok(())
    }

    fn remove_edge(&mut self, from: &TaskRef, to: &TaskRef, kind: EdgeKind) -> Result<(), EditRefusal> {
        let a = self.resolve(from)?;
        let b = self.resolve(to)?;
        self.edge_ends(&a, &b)?;
        self.not_held(&a)?;
        self.require_not_started(&a, StartedChange::Reordered)?;
        let slug_a = self.node(&a)?.slug.clone();
        let slug_b = self.node(&b)?.slug.clone();
        let definition = &mut self.node_mut(&a)?.definition;
        let removed = match kind {
            EdgeKind::After => {
                let before = definition.after.len();
                definition.after.retain(|edge| edge.task != slug_b);
                definition.after.len() != before
            }
            EdgeKind::StackedOn => {
                let names = definition
                    .stacked_on
                    .as_ref()
                    .is_some_and(|edge| edge.task == slug_b);
                if names {
                    definition.stacked_on = None;
                }
                names
            }
        };
        if !removed {
            return Err(EditRefusal::NoEdge {
                a: slug_a,
                b: slug_b,
                kind,
            });
        }
        self.touched.insert(a);
        Ok(())
    }

    fn finish(self) -> Result<Applied, EditRefusal> {
        let mut ops: Vec<DefinitionOp> = self
            .moves
            .iter()
            .map(|(from, to)| DefinitionOp::Move {
                from: from.clone(),
                to: to.clone(),
            })
            .collect();
        let at = |folder: &str| -> Result<RelPath, EditRefusal> {
            let path = if folder.is_empty() {
                TASK_FILE.to_string()
            } else {
                format!("{folder}/{TASK_FILE}")
            };
            RelPath::new(path.clone())
                .ok_or_else(|| EditRefusal::Internal(format!("{path} is not a definitions path")))
        };
        let index_folder: BTreeMap<&str, &str> = self
            .index
            .iter()
            .map(|entry| (entry.id.as_str(), entry.folder.as_str()))
            .collect();
        let mut writes = Vec::new();
        for node in self.tree.nodes() {
            let id = node.definition.id.as_str();
            let unchanged = self.original.get(id) == Some(&node.definition)
                && index_folder.get(id) == Some(&node.folder.as_str());
            if unchanged {
                continue;
            }
            let bytes = toml::to_string(&node.definition).map_err(|error| {
                EditRefusal::Internal(format!("writing the definition of {id}: {error}"))
            })?;
            writes.push((at(&node.folder)?, bytes.into_bytes()));
        }
        writes.sort_by(|a, b| a.0.cmp(&b.0));
        let written: BTreeSet<&RelPath> = writes.iter().map(|(path, _)| path).collect();
        let mut deletes = Vec::new();
        for entry in self.index.iter().filter(|entry| entry.removed) {
            let path = at(&entry.folder)?;
            if !written.contains(&path) {
                deletes.push(path);
            }
        }
        deletes.sort();
        ops.extend(deletes.into_iter().map(|path| DefinitionOp::Delete { path }));
        ops.extend(
            writes
                .into_iter()
                .map(|(path, bytes)| DefinitionOp::Write { path, bytes }),
        );
        Ok(Applied {
            ops,
            created: self.created,
            touched: self.touched,
        })
    }
}

fn find_mut<'n>(node: &'n mut DefinitionNode, id: &str) -> Option<&'n mut DefinitionNode> {
    if node.definition.id == id {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find_map(|child| find_mut(child, id))
}

fn detach(node: &mut DefinitionNode, id: &str) -> Option<DefinitionNode> {
    if let Some(position) = node
        .children
        .iter()
        .position(|child| child.definition.id == id)
    {
        return Some(node.children.remove(position));
    }
    node.children
        .iter_mut()
        .find_map(|child| detach(child, id))
}

fn refolder(node: &mut DefinitionNode, folder: String) {
    node.slug = split_folder(&folder).1.to_string();
    for child in &mut node.children {
        let child_folder = child_folder(&folder, &child.slug);
        refolder(child, child_folder);
    }
    node.folder = folder;
}

fn collect_ids(node: &DefinitionNode, ids: &mut Vec<TaskId>) {
    ids.push(node.definition.id.clone());
    for child in &node.children {
        collect_ids(child, ids);
    }
}

/// The parent folder and slug of a task folder; `("", "")` for the top group.
fn split_folder(folder: &str) -> (&str, &str) {
    let (rest, slug) = folder.rsplit_once('/').unwrap_or(("", folder));
    let parent = rest
        .strip_suffix(TASKS_DIR)
        .unwrap_or(rest)
        .trim_end_matches('/');
    (parent, slug)
}

fn holds_edge_to(definition: &TaskDefinition, slug: &str) -> bool {
    definition.after.iter().any(|edge| edge.task == slug)
        || definition
            .stacked_on
            .as_ref()
            .is_some_and(|edge| edge.task == slug)
}

/// The chain a new edge from the child `from` of `parent` to its sibling `target` (a slug) would
/// close, by `rules::cycle` over the sibling group.
fn sibling_cycle(parent: &DefinitionNode, from: &str, target: &str) -> Option<Vec<String>> {
    let siblings: Vec<Task> = parent
        .children
        .iter()
        .map(|child| Task {
            slug: child.slug.clone(),
            dir: PathBuf::from(&child.folder),
            definition: Some(PathBuf::from(&child.folder)),
            pending: None,
            needs_conversion: false,
            done_hold: None,
            file: Some(join(child.definition.clone(), TaskStateRecord::default())),
            artifacts: None,
            errors: Vec::new(),
            broken_edges: Vec::new(),
            files: BTreeMap::new(),
            hashes: BTreeMap::new(),
            children: Vec::new(),
        })
        .collect();
    let task = siblings
        .iter()
        .find(|task| task.id() == Some(from))?;
    let place = Place {
        task,
        siblings: &siblings,
        ancestors: Vec::new(),
        project: None,
    };
    let file = task.file.as_ref()?;
    rules::cycle(&place, file, target)
}

fn clean_work_kind(label: Option<&str>) -> Option<String> {
    label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
}

/// The creation time of a new task: the time in its UUID v7 id, to the second, so a rebuild with
/// the recorded id writes the same file. An id with no time gives the Unix epoch.
fn created_at(id: &str) -> Timestamp {
    let seconds = uuid::Uuid::parse_str(id)
        .ok()
        .and_then(|uuid| uuid.get_timestamp())
        .map_or(0, |time| time.to_unix().0);
    let time = i64::try_from(seconds)
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .unwrap_or(chrono::DateTime::UNIX_EPOCH);
    let text = time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // An RFC 3339 string with a `Z` offset is always a valid TOML datetime.
    text.parse()
        .unwrap_or_else(|_| unreachable!("{text} is a TOML datetime"))
}
