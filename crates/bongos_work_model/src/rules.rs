//! The rules the commands enforce, each returning the reason the UI shows when it refuses:
//! - a child goes under a group, or under a leaf with no worktree, branch or result (otherwise
//!   `task: split`);
//! - a leaf holds one result, except a `babysit` leaf, which holds a set of PRs;
//! - edges join siblings only, a task has at most one `stacked on`, both ends of a `stacked on`
//!   are stackable and in one repository, and no edge closes a cycle over `after` and
//!   `stacked on` (refused naming the chain, `cycle: a -> b -> c -> a`);
//! - the ships todo of a stacked task waits for its parent to be Done (`task: advance override`
//!   ships anyway and leaves a marker);
//! - dropping a group lists its open descendants and needs a confirm.
//!
//! Nothing here reads or writes a file.

use std::collections::HashSet;

use crate::derive::{self, Shape, State};
use crate::graph::state_label;
use crate::store::{Project, Task, WorkTree};
use crate::types::{Kind, ResultKind, Status, TaskFile, TaskResult, Todo, TodoRole};

/// Why a command was refused, in words the UI shows as they are.
pub type Refusal = String;

/// Why the goal of `file` cannot be changed here, or `None` when it can. A synced link (`linear`
/// and `linear_state` both set) takes its title from Linear. An unlinked task, a hand-set
/// `linear` alone and a pending create (`linear_state` without `linear`) keep an editable goal.
pub fn goal_locked(file: &TaskFile) -> Option<Refusal> {
    let identifier = file.linear.as_deref()?;
    file.linear_state.as_ref()?;
    Some(format!(
        "the title comes from Linear ({identifier}); change it there or run task: open in linear"
    ))
}

/// The branch name for a leaf with a synced Linear link: the one implementation, which the
/// worktree code and the Linear context text both call.
///
/// The identifier is lowercased. A `template` without `{linear}` (the default is `{slug}`) gets
/// `<id>-` in front of its last path segment, so `{slug}` gives `seq-121-proration-engine` and
/// `kyle/{slug}` gives `kyle/seq-121-proration-engine`. A template with `{linear}` expands it
/// and `{slug}` as given.
pub fn linear_branch_segment(identifier: &str, template: &str, slug: &str) -> String {
    let identifier = identifier.to_lowercase();
    if template.contains("{linear}") {
        return template
            .replace("{slug}", slug)
            .replace("{linear}", &identifier);
    }
    let expanded = template.replace("{slug}", slug);
    match expanded.rsplit_once('/') {
        Some((head, last)) => format!("{head}/{identifier}-{last}"),
        None => format!("{identifier}-{expanded}"),
    }
}

/// Template, slug, identifier and the branch they give, for the tests of every caller of
/// [`linear_branch_segment`] to share.
pub const LINEAR_BRANCH_VECTOR: [(&str, &str, &str, &str); 4] = [
    (
        "SEQ-121",
        "{slug}",
        "proration-engine",
        "seq-121-proration-engine",
    ),
    (
        "SEQ-121",
        "kyle/{slug}",
        "proration-engine",
        "kyle/seq-121-proration-engine",
    ),
    (
        "SEQ-121",
        "{linear}-{slug}",
        "proration-engine",
        "seq-121-proration-engine",
    ),
    (
        "SEQ-121",
        "kyle/{linear}/{slug}",
        "proration-engine",
        "kyle/seq-121/proration-engine",
    ),
];

/// The two kinds of ordering edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    After,
    StackedOn,
}

/// Where a task sits in the tree: its siblings (the tasks sharing its parent directory), its
/// ancestors and its project.
pub struct Place<'a> {
    pub task: &'a Task,
    /// The task and the tasks sharing its parent directory. A project's top task has none but
    /// itself.
    pub siblings: &'a [Task],
    /// The groups above the task, parent first.
    pub ancestors: Vec<&'a Task>,
    /// The project the task is in; `None` for a one-off and its descendants.
    pub project: Option<&'a Project>,
}

impl<'a> Place<'a> {
    /// Finds the task with `id` in `tree`.
    pub fn locate(tree: &'a WorkTree, id: &str) -> Result<Self, Refusal> {
        for project in &tree.projects {
            let mut ancestors = Vec::new();
            if let Some((task, siblings)) =
                find(std::slice::from_ref(&project.task), id, &mut ancestors)
            {
                ancestors.reverse();
                return Ok(Self {
                    task,
                    siblings,
                    ancestors,
                    project: Some(project),
                });
            }
        }
        for activity in &tree.activities {
            let mut ancestors = Vec::new();
            if let Some((task, siblings)) = find(&activity.oneoffs, id, &mut ancestors) {
                ancestors.reverse();
                return Ok(Self {
                    task,
                    siblings,
                    ancestors,
                    project: None,
                });
            }
        }
        Err(format!("no task has the id {id}"))
    }

    pub fn slug(&self) -> &'a str {
        &self.task.slug
    }

    /// Whether the task is a project's top group.
    pub fn is_project(&self) -> bool {
        self.project
            .is_some_and(|project| project.task.dir == self.task.dir)
    }

    pub fn sibling(&self, slug: &str) -> Option<&'a Task> {
        self.siblings
            .iter()
            .find(|task| task.slug == slug && task.dir != self.task.dir)
    }

    /// The siblings whose `stacked_on` names this task.
    pub fn stacked_on_this(&self) -> Vec<&'a Task> {
        self.siblings
            .iter()
            .filter(|task| {
                task.dir != self.task.dir
                    && task
                        .file
                        .as_ref()
                        .and_then(|file| file.stacked_on.as_ref())
                        .is_some_and(|edge| edge.task == self.task.slug)
            })
            .collect()
    }

    /// The repository a sibling of this task inherits when its file names none: the nearest
    /// ancestor's, then the project's when it has exactly one.
    pub fn inherited_repository(&self) -> Option<String> {
        let from_ancestors = if self.is_project() {
            None
        } else {
            self.ancestors
                .iter()
                .find_map(|task| task.file.as_ref()?.repository.clone())
        };
        from_ancestors.or_else(|| {
            let repositories = &self.project?.file.as_ref()?.repositories;
            match repositories.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            }
        })
    }

    /// The repository `file` binds to at this place.
    pub fn repository(&self, file: &TaskFile) -> Option<String> {
        file.repository
            .clone()
            .or_else(|| self.inherited_repository())
    }
}

fn find<'a>(
    siblings: &'a [Task],
    id: &str,
    ancestors: &mut Vec<&'a Task>,
) -> Option<(&'a Task, &'a [Task])> {
    for task in siblings {
        if task.id() == Some(id) {
            return Some((task, siblings));
        }
        ancestors.push(task);
        if let Some(found) = find(&task.children, id, ancestors) {
            return Some(found);
        }
        ancestors.pop();
    }
    None
}

/// Whether `file` has a `produces-branch` todo and a `ships` todo, whatever they are named.
pub fn file_stackable(file: &TaskFile) -> bool {
    has_role(&file.todos, TodoRole::ProducesBranch) && has_role(&file.todos, TodoRole::Ships)
}

fn has_role(todos: &[Todo], role: TodoRole) -> bool {
    todos.iter().any(|todo| todo.role == Some(role))
}

pub fn role_name(role: TodoRole) -> &'static str {
    match role {
        TodoRole::ProducesBranch => "produces-branch",
        TodoRole::Ships => "ships",
        TodoRole::Plans => "plans",
    }
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A child goes under a group, or under a leaf with no worktree, branch or result; a leaf with any
/// of those gains children only through `task: split`. A leaf in a stack cannot become a group
/// either, because a group is never stackable.
pub fn check_add_child(place: &Place, tree: &WorkTree) -> Result<(), Refusal> {
    let task = place.task;
    if derive::shape(task, tree) == Shape::Group {
        return Ok(());
    }
    let Some(file) = &task.file else {
        return Ok(());
    };
    let mut has = Vec::new();
    if file.worktree.is_some() {
        has.push("a worktree");
    }
    if file.branch.is_some() {
        has.push("a branch");
    }
    if file.result.is_some() {
        has.push("a result");
    }
    if !has.is_empty() {
        return Err(format!(
            "{} is a leaf with {}; use task: split",
            task.slug,
            join_and(&has)
        ));
    }
    check_not_stacked(place, file)
}

/// `task: split` turns a leaf into a group with one child that takes its work; a group is refused,
/// and so is a leaf in a stack, because a group is never stackable.
pub fn check_split(place: &Place, tree: &WorkTree) -> Result<(), Refusal> {
    let task = place.task;
    if derive::shape(task, tree) == Shape::Group {
        return Err(format!(
            "{} is already a group; use task: new child",
            task.slug
        ));
    }
    match &task.file {
        Some(file) => check_not_stacked(place, file),
        None => Ok(()),
    }
}

/// A task about to become a group is in no stack, at either end.
pub fn check_not_stacked(place: &Place, file: &TaskFile) -> Result<(), Refusal> {
    let task = place.task;
    if let Some(edge) = &file.stacked_on {
        return Err(format!(
            "{} is stacked on {}, and a group cannot be stacked; remove that edge first",
            task.slug, edge.task
        ));
    }
    if let Some(child) = place.stacked_on_this().first() {
        return Err(format!(
            "{} is stacked on {}, and a group cannot be stacked; remove that edge first",
            child.slug, task.slug
        ));
    }
    Ok(())
}

/// One result per leaf. Setting the same result again (same kind and URL) updates it, which is
/// how a PR's state moves from open to merged. A `babysit` leaf holds a set of PRs instead.
pub fn check_set_result(slug: &str, file: &TaskFile, result: &TaskResult) -> Result<(), Refusal> {
    if file.kind == Kind::Babysit {
        if matches!(result.kind, ResultKind::Pr | ResultKind::Prs) {
            return Ok(());
        }
        return Err(format!(
            "{slug} is a babysit task; its result is a set of PRs"
        ));
    }
    match &file.result {
        Some(existing) if existing.kind != result.kind || existing.url != result.url => {
            Err(format!("{slug} already has a result; clear it first"))
        }
        _ => Ok(()),
    }
}

/// The rules for a new edge from the task at `place` to `target`. `file` is the task's file as it
/// stands before the edge is added (for `task: retarget`, without its old `stacked_on`).
pub fn check_edge(
    place: &Place,
    file: &TaskFile,
    target: &Task,
    kind: EdgeKind,
) -> Result<(), Refusal> {
    let slug = place.slug();
    if target.dir == place.task.dir {
        return Err(format!("{slug} cannot have an edge to itself"));
    }
    if place.sibling(&target.slug).map(|task| &task.dir) != Some(&target.dir) {
        return Err(format!(
            "edges join siblings only: {slug} and {} have different parents",
            target.slug
        ));
    }
    let has_edge = file.after.iter().any(|edge| edge.task == target.slug)
        || file
            .stacked_on
            .as_ref()
            .is_some_and(|edge| edge.task == target.slug);
    if has_edge {
        return Err(format!("{slug} already has an edge to {}", target.slug));
    }
    if kind == EdgeKind::StackedOn {
        if let Some(existing) = &file.stacked_on {
            return Err(format!(
                "{slug} is already stacked on {}; a task has at most one stacked on",
                existing.task
            ));
        }
        let target_stackable = target.file.as_ref().is_some_and(file_stackable);
        for (name, stackable) in [
            (slug, file_stackable(file)),
            (target.slug.as_str(), target_stackable),
        ] {
            if !stackable {
                return Err(format!(
                    "stacked on needs both tasks stackable; {name} needs a produces-branch todo \
                     and a ships todo"
                ));
            }
        }
        let own = place.repository(file);
        let other = target
            .file
            .as_ref()
            .and_then(|file| file.repository.clone())
            .or_else(|| place.inherited_repository());
        if own != other {
            return Err(repository_mismatch(slug, &own, &target.slug, &other));
        }
    }
    if let Some(chain) = cycle(place, file, &target.slug) {
        return Err(format!("cycle: {}", chain.join(" -> ")));
    }
    Ok(())
}

fn repository_mismatch(
    slug: &str,
    own: &Option<String>,
    other_slug: &str,
    other: &Option<String>,
) -> Refusal {
    let name = |repository: &Option<String>| {
        repository
            .clone()
            .unwrap_or_else(|| "no repository".to_string())
    };
    format!(
        "stacked on needs both tasks in one repository; {slug} is in {}, {other_slug} in {}",
        name(own),
        name(other)
    )
}

/// A new repository for the task at `place` keeps every stack it is in within one repository,
/// and a task with a worktree keeps its repository.
pub fn check_repository_change(
    place: &Place,
    file: &TaskFile,
    repository: &Option<String>,
) -> Result<(), Refusal> {
    let slug = place.slug();
    let new = repository.clone().or_else(|| place.inherited_repository());
    if file.worktree.is_some() && new != place.repository(file) {
        return Err(format!(
            "{slug} already has a worktree, so its repository cannot change"
        ));
    }
    let sibling_repository = |task: &Task| {
        task.file
            .as_ref()
            .and_then(|file| file.repository.clone())
            .or_else(|| place.inherited_repository())
    };
    if let Some(target) = file
        .stacked_on
        .as_ref()
        .and_then(|edge| place.sibling(&edge.task))
    {
        let other = sibling_repository(target);
        if other != new {
            return Err(repository_mismatch(slug, &new, &target.slug, &other));
        }
    }
    for child in place.stacked_on_this() {
        let other = sibling_repository(child);
        if other != new {
            return Err(repository_mismatch(slug, &new, &child.slug, &other));
        }
    }
    Ok(())
}

/// The chain closed by a new edge from the task at `place` to `target`, such as `[a, b, c, a]`:
/// the edges `a -> b` (new), `b -> c` and `c -> a`. `file` stands in for the task's own file.
pub fn cycle(place: &Place, file: &TaskFile, target: &str) -> Option<Vec<String>> {
    let from = place.slug();
    let targets = |slug: &str| -> Vec<String> {
        let file = if slug == from {
            Some(file)
        } else {
            place.sibling(slug).and_then(|task| task.file.as_ref())
        };
        file.map(|file| {
            file.after
                .iter()
                .map(|edge| edge.task.clone())
                .chain(file.stacked_on.iter().map(|edge| edge.task.clone()))
                .collect()
        })
        .unwrap_or_default()
    };

    fn walk(
        node: &str,
        goal: &str,
        targets: &dyn Fn(&str) -> Vec<String>,
        path: &mut Vec<String>,
        seen: &mut HashSet<String>,
    ) -> bool {
        path.push(node.to_string());
        if node == goal {
            return true;
        }
        if seen.insert(node.to_string()) {
            for next in targets(node) {
                if walk(&next, goal, targets, path, seen) {
                    return true;
                }
            }
        }
        path.pop();
        false
    }

    let mut path = vec![from.to_string()];
    let mut seen = HashSet::new();
    walk(target, from, &targets, &mut path, &mut seen).then_some(path)
}

/// When the todo at `index` is the ships todo of a task stacked on a sibling that is not Done:
/// that sibling and its state. A stacked-on edge naming no sibling holds nothing back.
pub fn ship_waits_on<'a>(
    place: &Place<'a>,
    tree: &WorkTree,
    file: &TaskFile,
    index: usize,
) -> Option<(&'a Task, State)> {
    let todo = file.todos.get(index)?;
    if todo.role != Some(TodoRole::Ships) {
        return None;
    }
    let parent = place.sibling(&file.stacked_on.as_ref()?.task)?;
    let state = derive::state(parent, tree);
    (state != State::Done).then_some((parent, state))
}

/// The refusal for shipping the task at `place` before `parent` is Done.
pub fn ship_refusal(place: &Place, parent: &Task, state: State) -> Refusal {
    format!(
        "{} cannot ship before {} is Done (it is {}); use task: advance override",
        place.slug(),
        parent.slug,
        state_label(state)
    )
}

/// The descendants of `task` that are still open, which `task: drop` cancels with it.
pub fn open_descendants<'a>(task: &'a Task, tree: &WorkTree) -> Vec<&'a Task> {
    task.walk()
        .into_iter()
        .skip(1)
        .filter(|descendant| {
            descendant
                .file
                .as_ref()
                .is_some_and(|file| file.status == Status::Open)
                && !matches!(
                    derive::state(descendant, tree),
                    State::Done | State::Cancelled
                )
        })
        .collect()
}

/// Dropping a group with open descendants needs a confirm, and the refusal lists them.
pub fn check_drop(task: &Task, tree: &WorkTree, confirmed: bool) -> Result<(), Refusal> {
    let open = open_descendants(task, tree);
    if confirmed || open.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = open.iter().map(|task| task.slug.as_str()).collect();
    Err(format!(
        "dropping {} also cancels its open tasks: {}; confirm to drop them all",
        task.slug,
        names.join(", ")
    ))
}

/// A task in a stack keeps a produces-branch todo and a ships todo. `before` and `after` are its
/// file around the change; a task that was not stackable before is not held to this.
pub fn check_stays_stackable(
    place: &Place,
    before: &TaskFile,
    after: &TaskFile,
) -> Result<(), Refusal> {
    if !file_stackable(before) || file_stackable(after) {
        return Ok(());
    }
    let slug = place.slug();
    if let Some(edge) = &after.stacked_on {
        return Err(format!(
            "{slug} is stacked on {}, so it must keep a produces-branch todo and a ships todo",
            edge.task
        ));
    }
    if let Some(child) = place.stacked_on_this().first() {
        return Err(format!(
            "{} is stacked on {slug}, so {slug} must keep a produces-branch todo and a ships todo",
            child.slug
        ));
    }
    Ok(())
}

/// A task has at most one todo of each role. `index` is the todo taking `role`, or `None` for a
/// new todo.
pub fn check_role_free(
    slug: &str,
    todos: &[Todo],
    index: Option<usize>,
    role: TodoRole,
) -> Result<(), Refusal> {
    let holder = todos
        .iter()
        .enumerate()
        .find(|(other, todo)| Some(*other) != index && todo.role == Some(role));
    match holder {
        Some((_, todo)) => Err(format!(
            "{slug} already has a {} todo ({})",
            role_name(role),
            todo.name
        )),
        None => Ok(()),
    }
}
