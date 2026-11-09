//! Applying and rejecting a proposal (D7 steps 8 to 10): the diff turned into one feature-03
//! [`Change`], which `commands::apply` writes through one `.op.toml` journal, so a crash leaves the
//! old tree, the new tree, or a journal the loader finishes; never half a tree.
//!
//! The change holds every file the apply writes (the new `task.toml`s, the rewritten ones, the
//! root's `[[proposals]]` record) and every rename (the moved tasks, then the `chats/` and `runs/`
//! of split leaves). Only the operations of the diff are carried out: a `Kept` or `Untouched`
//! operation writes nothing, and a done or cancelled task, or one under such a task, is never
//! written. Every task the apply writes gets exactly one history entry, `proposal applied`, saying
//! what changed on it; the root's is the summary line the toast shows.
//!
//! The writer runs [`checked`] (or [`Request::run`]) on the tree it holds: the diff is computed
//! again and must have the fingerprint the preview showed, else nothing is written.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use bongos_work_model::commands::{Change, Context, Move, Outcome, Write, free_slug, split_work};
use bongos_work_model::derive::{self, State};
use bongos_work_model::layout;
use bongos_work_model::rules::{EdgeKind, Place, Refusal, role_name};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{
    Edge, Extra, HistoryEntry, ProposalRecord, ProposalState, StackedOn, Status, TaskFile, TaskId,
    TodoRole, TodoState, UnblockPoint,
};
use sha2::{Digest, Sha256};

use crate::diff::{Choices, Diff, NewTask, Op, TaskRef, TodoOp, diff, fingerprint};
use crate::grammar::{Parsed, kind_name, unblock_name};
use crate::preview::applied_summary;
use crate::print::LiveSet;

/// The refusal when the diff on the writer's tree is not the one previewed.
pub const STALE: &str = "The tree changed since the preview; run task: apply proposal again";

/// The history event of every task an apply writes.
pub const APPLIED_EVENT: &str = "proposal applied";

/// The event of a reject. It is the change's entry, but no history is written: a reject writes
/// only the record.
pub const REJECTED_EVENT: &str = "proposal rejected";

/// The SHA-256 of a proposal file's bytes in lowercase hex, as `[[proposals]]` records it.
pub fn file_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What the app hands the serial writer after the user confirms the preview: what was previewed,
/// never what is true now. The writer supplies the present: [`Request::run`] takes the live work
/// and the proposal file's bytes, both collected at confirm time, so a chat started or a file
/// edited since the preview changes the recomputed fingerprint and the apply is refused.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The proposal as parsed from the previewed file.
    pub parsed: Parsed,
    pub choices: Choices,
    /// The fingerprint of the previewed diff ([`fingerprint`]), over the previewed live work and
    /// the previewed file's hash.
    pub fingerprint: String,
}

impl Request {
    /// The writer's command: [`checked`] on the tree the writer holds, with the `live` work
    /// collected and the proposal file's `file_bytes` read when the user confirmed (the app reads
    /// them just before queueing the command; this crate stays free of the filesystem and gpui).
    pub fn run(
        &self,
        tree: &WorkTree,
        ctx: &Context,
        live: &LiveSet,
        file_bytes: &[u8],
    ) -> Outcome {
        checked(
            &self.parsed,
            tree,
            ctx,
            live,
            &self.choices,
            &self.fingerprint,
            &file_hash(file_bytes),
        )
    }
}

/// Recomputes the diff of `parsed` on `tree` and, when its fingerprint is the `previewed` one,
/// returns the change that applies it. Otherwise refuses with [`STALE`], and nothing is written.
pub fn checked(
    parsed: &Parsed,
    tree: &WorkTree,
    ctx: &Context,
    live: &LiveSet,
    choices: &Choices,
    previewed: &str,
    file_hash: &str,
) -> Outcome {
    let diff = diff(parsed, tree, live, choices);
    if fingerprint(&diff, choices, file_hash) != previewed {
        return Err(STALE.to_string());
    }
    change(parsed, tree, &diff, ctx, file_hash)
}

/// The change that applies `diff`, the diff of `parsed` against `tree`. See [`applied`].
pub fn change(
    parsed: &Parsed,
    tree: &WorkTree,
    diff: &Diff,
    ctx: &Context,
    file_hash: &str,
) -> Outcome {
    applied(parsed, tree, diff, ctx, file_hash).map(|applied| applied.change)
}

/// An apply before it is written.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    pub change: Change,
    /// The id given to each task the proposal creates, by its proposal line.
    pub created: BTreeMap<usize, TaskId>,
    /// The line the toast shows, also the root's history detail ([`applied_summary`]).
    pub summary: String,
}

/// Builds the one change that applies `diff`: refused when the diff is refused (an error badge),
/// when `parsed` has errors, or when the diff does not fit `tree` (it was computed on another).
pub fn applied(
    parsed: &Parsed,
    tree: &WorkTree,
    diff: &Diff,
    ctx: &Context,
    file_hash: &str,
) -> Result<Applied, Refusal> {
    if let Some(refusal) = &diff.refusal {
        return Err(refusal.clone());
    }
    if parsed.has_errors() {
        return Err("the proposal has errors; fix them first".to_string());
    }
    let number = proposal_number(parsed)?;
    if parsed.root.attributes.id.as_deref() != Some(diff.root.as_str()) {
        return Err("the diff is not of this proposal".to_string());
    }
    let mut builder = Builder::new(tree, ctx, number);
    // A done or cancelled root (or one under such a task) takes no proposal: not even its record.
    if let Some(word) = builder.closed.get(diff.root.as_str()) {
        let slug = tree.task_by_id(&diff.root).map_or("the root", |t| &t.slug);
        return Err(format!(
            "{slug} is {word}, and a {word} task is never changed"
        ));
    }
    builder.edit(&diff.root)?;
    builder.allocate(diff)?;
    builder.carry_out(diff)?;
    let summary = applied_summary(number, diff);
    let created = builder
        .created
        .iter()
        .map(|(line, created)| (*line, created.file.id.clone()))
        .collect();
    let change = builder.finish(&diff.root, &summary, file_hash)?;
    Ok(Applied {
        change,
        created,
        summary,
    })
}

/// `task: reject proposal`: the root's `[[proposals]]` record of proposal `number`, state
/// `rejected`, with the file's hash. Nothing else is written, not even history.
pub fn reject(tree: &WorkTree, ctx: &Context, root: &str, number: u32, file_hash: &str) -> Outcome {
    let task = tree
        .task_by_id(root)
        .ok_or_else(|| format!("no task has the id {root}"))?;
    task.check_writable()
        .map_err(|refused| refused.to_string())?;
    let mut file = task
        .file
        .clone()
        .ok_or_else(|| format!("{} has no task.toml", task.slug))?;
    record(&mut file, ctx, number, ProposalState::Rejected, file_hash);
    let entry = history_entry(ctx, REJECTED_EVENT, format!("proposal {number} rejected"));
    Ok(Change {
        dir: task.dir.clone(),
        writes: vec![Write::Task {
            dir: task.dir.clone(),
            file,
        }],
        entry,
        moves: Vec::new(),
    })
}

fn proposal_number(parsed: &Parsed) -> Result<u32, Refusal> {
    parsed
        .header
        .number
        .as_ref()
        .map(|number| number.value)
        .ok_or_else(|| "the proposal has no number".to_string())
}

/// Sets the record of proposal `number` on `file`, replacing an earlier one of that number (a
/// re-apply of an edited file).
fn record(file: &mut TaskFile, ctx: &Context, number: u32, state: ProposalState, hash: &str) {
    let record = ProposalRecord {
        number,
        state,
        at: ctx.at,
        by: ctx.by.clone(),
        hash: hash.to_string(),
        extra: Extra::new(),
    };
    match file
        .proposals
        .iter_mut()
        .find(|existing| existing.number == number)
    {
        Some(existing) => {
            let extra = std::mem::take(&mut existing.extra);
            *existing = ProposalRecord { extra, ..record };
        }
        None => file.proposals.push(record),
    }
}

fn history_entry(ctx: &Context, event: &str, detail: String) -> HistoryEntry {
    HistoryEntry {
        at: ctx.at,
        by: ctx.by.clone(),
        event: event.to_string(),
        detail: Some(detail),
        extra: Extra::new(),
    }
}

/// An existing task the apply rewrites.
struct Edited<'a> {
    task: &'a Task,
    file: TaskFile,
    /// What changed on it, for its history entry.
    parts: Vec<String>,
}

/// A task the apply creates.
struct Created {
    slug: String,
    dir: PathBuf,
    parent: TaskRef,
    task: NewTask,
    file: TaskFile,
    parts: Vec<String>,
}

struct Builder<'a> {
    tree: &'a WorkTree,
    ctx: &'a Context,
    number: u32,
    tasks: HashMap<&'a str, &'a Task>,
    /// The tasks that are done or cancelled, or under such a task.
    closed: HashMap<&'a str, &'static str>,
    edited: Vec<Edited<'a>>,
    created: BTreeMap<usize, Created>,
    /// The moved task directories, from where they are on disk.
    task_moves: Vec<(PathBuf, PathBuf)>,
    /// The `chats/` and `runs/` of split leaves, by their paths on disk.
    work_moves: Vec<Move>,
    /// The slugs taken under each parent directory (after the moves), on disk or by this apply.
    taken: HashMap<PathBuf, Vec<String>>,
    /// The slug of every moved task at its new place.
    moved_slugs: HashMap<TaskId, String>,
}

impl<'a> Builder<'a> {
    fn new(tree: &'a WorkTree, ctx: &'a Context, number: u32) -> Self {
        let mut tasks = HashMap::new();
        let mut closed = HashMap::new();
        fn visit<'a>(
            task: &'a Task,
            inherited: Option<&'static str>,
            tree: &'a WorkTree,
            tasks: &mut HashMap<&'a str, &'a Task>,
            closed: &mut HashMap<&'a str, &'static str>,
        ) {
            let own = match derive::state(task, tree) {
                State::Done => Some("done"),
                State::Cancelled => Some("cancelled"),
                _ => None,
            };
            let word = own.or(inherited);
            if let Some(id) = task.id() {
                tasks.insert(id, task);
                if let Some(word) = word {
                    closed.insert(id, word);
                }
            }
            for child in &task.children {
                visit(child, word, tree, tasks, closed);
            }
        }
        for project in &tree.projects {
            visit(&project.task, None, tree, &mut tasks, &mut closed);
        }
        for activity in &tree.activities {
            for oneoff in &activity.oneoffs {
                visit(oneoff, None, tree, &mut tasks, &mut closed);
            }
        }
        Self {
            tree,
            ctx,
            number,
            tasks,
            closed,
            edited: Vec::new(),
            created: BTreeMap::new(),
            task_moves: Vec::new(),
            work_moves: Vec::new(),
            taken: HashMap::new(),
            moved_slugs: HashMap::new(),
        }
    }

    fn task(&self, id: &str) -> Result<&'a Task, Refusal> {
        self.tasks
            .get(id)
            .copied()
            .ok_or_else(|| format!("no task has the id {id}; the tree changed"))
    }

    /// The task with `id`, to rewrite: its write guard is checked once, on first use.
    fn edit(&mut self, id: &str) -> Result<&mut Edited<'a>, Refusal> {
        let at = match self
            .edited
            .iter()
            .position(|edited| edited.task.id() == Some(id))
        {
            Some(at) => at,
            None => {
                let task = self.task(id)?;
                task.check_writable()
                    .map_err(|refused| refused.to_string())?;
                let file = task
                    .file
                    .clone()
                    .ok_or_else(|| format!("{} has no task.toml", task.slug))?;
                self.edited.push(Edited {
                    task,
                    file,
                    parts: Vec::new(),
                });
                self.edited.len() - 1
            }
        };
        Ok(&mut self.edited[at])
    }

    fn created(&mut self, line: usize) -> Result<&mut Created, Refusal> {
        self.created
            .get_mut(&line)
            .ok_or_else(|| format!("line {line} creates no task"))
    }

    /// Where the directory `disk` is once the moved tasks have moved.
    fn current(&self, disk: &Path) -> PathBuf {
        self.task_moves
            .iter()
            .find_map(|(from, to)| disk.strip_prefix(from).ok().map(|rest| to.join(rest)))
            .unwrap_or_else(|| disk.to_path_buf())
    }

    fn dir_of(&self, task: &TaskRef) -> Result<PathBuf, Refusal> {
        match task {
            TaskRef::Existing { id, .. } => Ok(self.current(&self.task(id)?.dir)),
            TaskRef::New { line, .. } => self
                .created
                .get(line)
                .map(|created| created.dir.clone())
                .ok_or_else(|| format!("line {line} creates no task")),
        }
    }

    /// The slug of `task` after the apply.
    fn slug_of(&self, task: &TaskRef) -> Result<String, Refusal> {
        match task {
            TaskRef::Existing { id, slug } => Ok(self
                .moved_slugs
                .get(id)
                .cloned()
                .unwrap_or_else(|| slug.clone())),
            TaskRef::New { line, .. } => self
                .created
                .get(line)
                .map(|created| created.slug.clone())
                .ok_or_else(|| format!("line {line} creates no task")),
        }
    }

    /// A free slug from `base` under `parent`, taken at once. Every slug on disk there counts as
    /// taken, also those of tasks that move away, so no step of the journal renames into a path an
    /// earlier step freed: running it again after a crash would find both ends present.
    fn take_slug(&mut self, parent: &TaskRef, base: &str) -> Result<(PathBuf, String), Refusal> {
        let dir = self.dir_of(parent)?;
        let on_disk: Vec<String> = match parent {
            TaskRef::Existing { id, .. } => self
                .task(id)?
                .children
                .iter()
                .map(|child| child.slug.clone())
                .collect(),
            TaskRef::New { .. } => Vec::new(),
        };
        let taken = self.taken.entry(dir.clone()).or_insert(on_disk);
        let names: Vec<&str> = taken.iter().map(String::as_str).collect();
        let slug = free_slug(base, &names);
        taken.push(slug.clone());
        Ok((layout::child_dir(&dir, &slug), slug))
    }

    /// The repository a task under `parent` inherits after the apply.
    fn inherited_repository(&self, parent: &TaskRef) -> Result<Option<String>, Refusal> {
        let mut at = parent.clone();
        loop {
            match at {
                TaskRef::Existing { id, .. } => {
                    let task = self.task(&id)?;
                    let place = Place::locate(self.tree, &id)?;
                    return Ok(task.file.as_ref().and_then(|file| place.repository(file)));
                }
                TaskRef::New { line, .. } => {
                    let created = self
                        .created
                        .get(&line)
                        .ok_or_else(|| format!("line {line} creates no task"))?;
                    at = created.parent.clone();
                }
            }
        }
    }

    /// Gives every created and moved task its slug and directory, in proposal-line order, so a
    /// parent is placed before its children.
    fn allocate(&mut self, diff: &Diff) -> Result<(), Refusal> {
        for op in &diff.ops {
            match op {
                Op::Create {
                    line,
                    parent,
                    label,
                    task,
                } => {
                    let (dir, slug) = self.take_slug(parent, label)?;
                    let mut file = TaskFile::new(task.goal.clone(), task.kind);
                    file.created = self.ctx.at;
                    file.todos = task.todos.clone();
                    file.planned_result = task.planned_result.clone();
                    self.created.insert(
                        *line,
                        Created {
                            slug,
                            dir,
                            parent: parent.clone(),
                            task: task.clone(),
                            file,
                            parts: vec!["created".to_string()],
                        },
                    );
                }
                Op::Move { id, new_parent, .. } => {
                    let task = self.task(id)?;
                    if id == &diff.root {
                        return Err(format!(
                            "{} is the proposal's root and cannot move",
                            task.slug
                        ));
                    }
                    // The whole subtree moves, so no task in it may carry an error badge.
                    for inner in task.walk() {
                        inner
                            .check_writable()
                            .map_err(|refused| refused.to_string())?;
                    }
                    let (to, slug) = self.take_slug(new_parent, &task.slug)?;
                    self.task_moves.push((task.dir.clone(), to));
                    self.moved_slugs.insert(id.clone(), slug);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Carries out every operation on the files, in an order that composes: moves, cancels, the
    /// edits of one task, splits, edge removals, edge additions, then the plan todo.
    fn carry_out(&mut self, diff: &Diff) -> Result<(), Refusal> {
        for op in &diff.ops {
            if let Op::Move {
                id,
                new_parent,
                unlinked,
                ..
            } = op
            {
                self.moved(id, new_parent, unlinked)?;
            }
        }
        for op in &diff.ops {
            if let Op::Cancel { id, with, .. } = op {
                self.cancel(id, with)?;
            }
        }
        for op in &diff.ops {
            match op {
                Op::Rename { id, from, to, .. } => {
                    let edited = self.edit(id)?;
                    edited.file.goal = to.clone();
                    edited.parts.push(format!("renamed \"{from}\" -> \"{to}\""));
                }
                Op::SetKind { id, from, kind, .. } => {
                    let edited = self.edit(id)?;
                    edited.file.kind = *kind;
                    edited
                        .parts
                        .push(format!("kind {} -> {}", kind_name(*from), kind_name(*kind)));
                }
                Op::SetPlannedResult { id, text, .. } => {
                    let edited = self.edit(id)?;
                    edited.file.planned_result = text.clone();
                    edited.parts.push(match text {
                        Some(text) => format!("planned result \"{text}\""),
                        None => "planned result removed".to_string(),
                    });
                }
                Op::Todos { id, ops, todos, .. } => {
                    let edited = self.edit(id)?;
                    edited.file.todos = todos.clone();
                    edited.parts.extend(ops.iter().map(todo_part));
                }
                _ => {}
            }
        }
        for op in &diff.ops {
            if let Op::Split { id, keeper, .. } = op {
                let plan_here = diff.ops.iter().any(
                    |op| matches!(op, Op::PlanDone { id: done, keeper: None, .. } if done == id),
                );
                match keeper {
                    Some(keeper) => self.split_into(id, keeper.line)?,
                    None => self.become_group(id, plan_here)?,
                }
            }
        }
        for op in &diff.ops {
            if let Op::RemoveEdge {
                id,
                target,
                kind,
                unblock,
                ..
            } = op
            {
                self.remove_edge(id, target, *kind, *unblock)?;
            }
        }
        for op in &diff.ops {
            if let Op::AddEdge {
                from,
                target,
                kind,
                unblock,
                default,
                ..
            } = op
            {
                self.add_edge(from, target, *kind, *unblock, *default)?;
            }
        }
        for op in &diff.ops {
            if let Op::PlanDone {
                id, todo, keeper, ..
            } = op
            {
                self.plan_done(id, todo, keeper.as_ref())?;
            }
        }
        Ok(())
    }

    fn shown(&self, dir: &Path) -> String {
        dir.strip_prefix(&self.tree.root)
            .unwrap_or(dir)
            .display()
            .to_string()
    }

    /// The moved task loses its own edges (they joined its old siblings), and the old siblings
    /// `unlinked` lose theirs to it, as feature 03's `task: move` does.
    fn moved(
        &mut self,
        id: &str,
        new_parent: &TaskRef,
        unlinked: &[TaskId],
    ) -> Result<(), Refusal> {
        let task = self.task(id)?;
        let old_slug = task.slug.clone();
        let to = self.current(&task.dir);
        let before =
            Place::locate(self.tree, id)?.repository(task.file.as_ref().ok_or("no task file")?);
        let inherited = self.inherited_repository(new_parent)?;
        let shown = format!("moved {} -> {}", self.shown(&task.dir), self.shown(&to));
        let edited = self.edit(id)?;
        let mut removed: Vec<String> = edited.file.after.drain(..).map(|edge| edge.task).collect();
        removed.extend(edited.file.stacked_on.take().map(|edge| edge.task));
        edited.file.activity = None;
        edited.parts.push(shown);
        if !removed.is_empty() {
            edited
                .parts
                .push(format!("edges removed: {}", removed.join(", ")));
        }
        // A worktree or branch stays in the repository it was made in.
        if edited.file.repository.is_none()
            && holds_inherited_branch(task)
            && before.is_some()
            && before != inherited
        {
            edited.file.repository = before.clone();
            if let Some(repository) = before {
                edited.parts.push(format!("repository kept: {repository}"));
            }
        }
        for sibling in unlinked {
            let edited = self.edit(sibling)?;
            edited.file.after.retain(|edge| edge.task != old_slug);
            if edited
                .file
                .stacked_on
                .as_ref()
                .is_some_and(|edge| edge.task == old_slug)
            {
                edited.file.stacked_on = None;
            }
            edited
                .parts
                .push(format!("edge to '{old_slug}' removed: it moved away"));
        }
        Ok(())
    }

    fn cancel(&mut self, id: &str, with: &[TaskId]) -> Result<(), Refusal> {
        let at = self.ctx.at;
        let slug = self.task(id)?.slug.clone();
        let mut names = Vec::new();
        for inner in with {
            let edited = self.edit(inner)?;
            edited.file.status = Status::Cancelled;
            edited.file.closed = Some(at);
            edited.parts.push(format!("dropped with {slug}"));
            names.push(edited.task.slug.clone());
        }
        let edited = self.edit(id)?;
        edited.file.status = Status::Cancelled;
        edited.file.closed = Some(at);
        edited.parts.push(if names.is_empty() {
            "dropped".to_string()
        } else {
            format!("dropped with {}", names.join(", "))
        });
        Ok(())
    }

    /// D8: the leaf `id` becomes a group and the keeper created on line `keeper` takes its work
    /// by feature 03's `split_work`. The keeper's todos are the reconciled list of the diff, which
    /// holds every frozen todo of the leaf exactly as it was.
    fn split_into(&mut self, id: &str, keeper: usize) -> Result<(), Refusal> {
        let task = self.task(id)?;
        let edited_at = {
            self.edit(id)?;
            self.edited
                .iter()
                .position(|edited| edited.task.id() == Some(id))
                .ok_or("the split leaf is not edited")?
        };
        let leaf_dir = self.current(&task.dir);
        let created = self
            .created
            .get_mut(&keeper)
            .ok_or_else(|| format!("line {keeper} creates no task"))?;
        let edited = &mut self.edited[edited_at];
        let (moved, moves) = split_work(&mut edited.file, &mut created.file, task, &created.dir);
        created.file.todos = created.task.todos.clone();
        created.file.planned_result = created.task.planned_result.clone();
        created.file.kind = created.task.kind;
        created.file.default_visibility = edited.file.default_visibility;
        let what = if moved.is_empty() {
            String::new()
        } else {
            format!("; moved: {}", moved.join(", "))
        };
        edited
            .parts
            .push(format!("split{what} into {}", created.slug));
        created
            .parts
            .push(format!("keeps the work of {}{what}", task.slug));
        // `split_work` names the directories where they are on disk; a moved leaf has moved.
        for work in moves {
            let rest = work
                .from
                .strip_prefix(&task.dir)
                .map_err(|_| "a split moves a directory outside its leaf".to_string())?;
            self.work_moves.push(Move {
                from: leaf_dir.join(rest),
                to: work.to,
            });
        }
        Ok(())
    }

    /// A leaf with no work becomes a group: its todos and planned result go to its history, except
    /// the plan todo this proposal completes, which is recorded done (D8, D9).
    fn become_group(&mut self, id: &str, plan_here: bool) -> Result<(), Refusal> {
        let edited = self.edit(id)?;
        let names: Vec<String> = edited
            .file
            .todos
            .iter()
            .filter(|todo| {
                !(plan_here && todo.role == Some(TodoRole::Plans) && todo.state == TodoState::Open)
            })
            .map(|todo| todo.name.clone())
            .collect();
        edited.file.todos.clear();
        let mut part = "became a group".to_string();
        if !names.is_empty() {
            part.push_str(&format!("; todos removed: {}", names.join(", ")));
        }
        if edited.file.planned_result.take().is_some() {
            part.push_str("; planned result removed");
        }
        edited.parts.push(part);
        Ok(())
    }

    fn remove_edge(
        &mut self,
        id: &str,
        target: &str,
        kind: EdgeKind,
        unblock: UnblockPoint,
    ) -> Result<(), Refusal> {
        let edited = self.edit(id)?;
        let found = match kind {
            EdgeKind::After => match edited
                .file
                .after
                .iter()
                .position(|edge| edge.task == target && edge.unblock == unblock)
            {
                Some(at) => {
                    edited.file.after.remove(at);
                    true
                }
                None => false,
            },
            EdgeKind::StackedOn => {
                let names = edited
                    .file
                    .stacked_on
                    .as_ref()
                    .is_some_and(|edge| edge.task == target && edge.unblock == unblock);
                if names {
                    edited.file.stacked_on = None;
                }
                names
            }
        };
        if !found {
            return Err(format!(
                "{} has no edge '{}'; the tree changed",
                edited.task.slug,
                edge_phrase(kind, target, unblock)
            ));
        }
        edited.parts.push(format!(
            "removed edge '{}'",
            edge_phrase(kind, target, unblock)
        ));
        Ok(())
    }

    fn add_edge(
        &mut self,
        from: &TaskRef,
        target: &TaskRef,
        kind: EdgeKind,
        unblock: UnblockPoint,
        default: bool,
    ) -> Result<(), Refusal> {
        let target = self.slug_of(target)?;
        let (file, parts) = match from {
            TaskRef::Existing { id, .. } => {
                let edited = self.edit(id)?;
                (&mut edited.file, &mut edited.parts)
            }
            TaskRef::New { line, .. } => {
                let created = self.created(*line)?;
                (&mut created.file, &mut created.parts)
            }
        };
        match kind {
            EdgeKind::After => file.after.push(Edge {
                task: target.clone(),
                unblock,
                extra: Extra::new(),
            }),
            EdgeKind::StackedOn => {
                if let Some(existing) = &file.stacked_on {
                    return Err(format!(
                        "a task is stacked on one task only; it is on {}",
                        existing.task
                    ));
                }
                file.stacked_on = Some(StackedOn {
                    task: target.clone(),
                    unblock,
                    extra: Extra::new(),
                });
            }
        }
        let mut part = format!("added edge '{}'", edge_phrase(kind, &target, unblock));
        if default {
            part.push_str(" (default)");
        }
        parts.push(part);
        Ok(())
    }

    fn plan_done(&mut self, id: &str, todo: &str, keeper: Option<&TaskRef>) -> Result<(), Refusal> {
        let is_plan = |candidate: &bongos_work_model::Todo| {
            candidate.role == Some(TodoRole::Plans)
                && candidate.state == TodoState::Open
                && candidate.name == todo
        };
        match keeper {
            Some(TaskRef::New { line, .. }) => {
                let created = self.created(*line)?;
                let found = created
                    .file
                    .todos
                    .iter_mut()
                    .find(|candidate| is_plan(candidate))
                    .ok_or_else(|| format!("the keeper has no open plan todo '{todo}'"))?;
                found.state = TodoState::Done;
                created.parts.push(format!("todo '{todo}' done"));
                let slug = created.slug.clone();
                self.edit(id)?
                    .parts
                    .push(format!("plan done: todo '{todo}' done in {slug}"));
            }
            Some(TaskRef::Existing { .. }) => {
                return Err("a plan todo moves only into a new keeper".to_string());
            }
            None => {
                let edited = self.edit(id)?;
                if let Some(found) = edited
                    .file
                    .todos
                    .iter_mut()
                    .find(|candidate| is_plan(candidate))
                {
                    found.state = TodoState::Done;
                    edited.parts.push(format!("plan done: todo '{todo}' done"));
                } else if edited.file.todos.is_empty() {
                    // The task became a group by this proposal; its plan todo is recorded here.
                    edited.parts.push(format!("plan done: todo '{todo}'"));
                } else {
                    return Err(format!(
                        "{} has no open plan todo '{todo}'",
                        edited.task.slug
                    ));
                }
            }
        }
        Ok(())
    }

    /// The change: the root first, with the record and the summary, then every other rewritten
    /// task, then the created ones; the task moves, then the moves of split work.
    fn finish(mut self, root: &str, summary: &str, hash: &str) -> Result<Change, Refusal> {
        let number = self.number;
        let ctx = self.ctx;
        let mut writes = Vec::new();
        let mut root_entry = None;
        for edited in &mut self.edited {
            let id = edited.task.id().unwrap_or_default();
            let is_root = id == root;
            if let Some(word) = self.closed.get(id)
                && (!is_root || !edited.parts.is_empty())
            {
                return Err(format!(
                    "{} is {word}, and a {word} task is never changed",
                    edited.task.slug
                ));
            }
            let detail = if is_root {
                let mut detail = summary.to_string();
                for part in &edited.parts {
                    detail.push_str("; ");
                    detail.push_str(part);
                }
                detail
            } else {
                format!("proposal {number}: {}", edited.parts.join("; "))
            };
            let entry = history_entry(ctx, APPLIED_EVENT, detail);
            edited.file.history.push(entry.clone());
            if is_root {
                record(&mut edited.file, ctx, number, ProposalState::Applied, hash);
                root_entry = Some(entry);
            }
        }
        let root_entry = root_entry.ok_or("the root is not written")?;
        let root_dir = self.task(root)?.dir.clone();
        // The root first.
        self.edited
            .sort_by_key(|edited| edited.task.id() != Some(root));
        for edited in &self.edited {
            writes.push(Write::Task {
                dir: self.current(&edited.task.dir),
                file: edited.file.clone(),
            });
        }
        for created in self.created.values_mut() {
            let entry = history_entry(
                ctx,
                APPLIED_EVENT,
                format!("proposal {number}: {}", created.parts.join("; ")),
            );
            created.file.history.push(entry);
            writes.push(Write::Task {
                dir: created.dir.clone(),
                file: created.file.clone(),
            });
        }
        // Task moves first: the journal goes in the first one's source parent, which no move
        // renames (one level of moves, D5), and the split work of a moved leaf moves from its new
        // place.
        let mut moves: Vec<Move> = self
            .task_moves
            .iter()
            .map(|(from, to)| Move {
                from: from.clone(),
                to: to.clone(),
            })
            .collect();
        moves.extend(self.work_moves);
        Ok(Change {
            dir: root_dir,
            writes,
            entry: root_entry,
            moves,
        })
    }
}

/// Whether `task`, or a descendant that inherits its repository from it, has a worktree or a
/// branch (feature 03's rule for `task: move`).
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

fn edge_phrase(kind: EdgeKind, target: &str, unblock: UnblockPoint) -> String {
    let word = match kind {
        EdgeKind::After => "after",
        EdgeKind::StackedOn => "stacked on",
    };
    let mut text = format!("{word} {target}");
    if unblock != bongos_work_model::commands::default_unblock(kind) {
        text.push_str(&format!(" | unblock {}", unblock_name(unblock)));
    }
    text
}

fn todo_part(op: &TodoOp) -> String {
    match op {
        TodoOp::Add { name, .. } => format!("added todo '{name}'"),
        TodoOp::Rename { from, to, .. } => format!("renamed todo '{from}' -> '{to}'"),
        TodoOp::SetRole { name, role } => {
            format!("todo '{name}' role {}", role.map_or("none", role_name))
        }
        TodoOp::SetWorkflow { name, workflow } => format!(
            "todo '{name}' workflow {}",
            workflow.as_deref().unwrap_or("none")
        ),
        TodoOp::Remove { name, .. } => format!("removed todo '{name}'"),
        TodoOp::Move { name, from, to } => {
            format!("moved todo '{name}' from {} to {}", from + 1, to + 1)
        }
    }
}

/// The ids of the existing tasks whose files `diff` changes, beyond the root (which always gets
/// the record). The apply writes exactly these, so the preview matches the apply.
pub fn rewritten(diff: &Diff) -> HashSet<TaskId> {
    let mut ids = HashSet::new();
    for op in &diff.ops {
        match op {
            Op::Rename { id, .. }
            | Op::SetKind { id, .. }
            | Op::SetPlannedResult { id, .. }
            | Op::Todos { id, .. }
            | Op::RemoveEdge { id, .. }
            | Op::Split { id, .. }
            | Op::PlanDone { id, .. } => {
                ids.insert(id.clone());
            }
            Op::AddEdge {
                from: TaskRef::Existing { id, .. },
                ..
            } => {
                ids.insert(id.clone());
            }
            Op::Cancel { id, with, .. } => {
                ids.insert(id.clone());
                ids.extend(with.iter().cloned());
            }
            Op::Move { id, unlinked, .. } => {
                ids.insert(id.clone());
                ids.extend(unlinked.iter().cloned());
            }
            Op::Create { .. } | Op::AddEdge { .. } | Op::Kept { .. } | Op::Untouched { .. } => {}
        }
    }
    ids
}
