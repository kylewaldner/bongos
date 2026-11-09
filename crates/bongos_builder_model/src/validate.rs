//! Validation of a parsed proposal against the work tree. The parser reports syntax; this adds the
//! rules that need the tree or the whole proposal: edges, stacks, groups, results, identity, `keeps
//! work`, workflows and the warnings. Nothing here touches the file system.

use std::collections::{BTreeSet, HashMap, HashSet};

use bongos_work_model::commands::default_todos;
use bongos_work_model::derive::{self, State};
use bongos_work_model::rules::{EdgeKind, Place, check_role_free, file_stackable};
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::{Extra, Kind, TaskFile, TodoRole, TodoState};

use crate::grammar::{Edge, Node, Parsed, Problem, Severity};

/// What the file is, as known from where it lives: `<task dir>/proposals/<number>.proposal.txt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposalFile {
    /// The number in the file name.
    pub number: u32,
    /// The id of the task whose directory holds the file.
    pub task_id: String,
}

/// The settings and sources validation depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checks {
    /// The known workflow names, once a registry exists (feature 20). `None` makes a workflow
    /// name a warning; `Some` makes an unknown one an error.
    pub workflows: Option<BTreeSet<String>>,
    /// A leaf with more todos than this gets a warning (`task_builder.todo_warning`).
    pub todo_warning: usize,
    /// The file's own number and task, for the `proposal N`, `for` and root `id` rules. Without
    /// it those rules are not checked, except that the root needs an id and `for` must equal it.
    pub file: Option<ProposalFile>,
}

impl Default for Checks {
    fn default() -> Self {
        Self {
            workflows: None,
            todo_warning: 8,
            file: None,
        }
    }
}

/// Every problem `validate` finds beyond the parser's, sorted by line (stable).
pub fn validate(parsed: &Parsed, tree: &WorkTree, checks: &Checks) -> Vec<Problem> {
    let mut validator = Validator {
        tree,
        checks,
        problems: Vec::new(),
        existing: HashMap::new(),
        have_root: false,
        ids_seen: HashMap::new(),
        labels: HashSet::new(),
    };
    validator.run(parsed);
    let mut problems = validator.problems;
    problems.sort_by_key(|problem| problem.line);
    problems
}

struct Validator<'a> {
    tree: &'a WorkTree,
    checks: &'a Checks,
    problems: Vec<Problem>,
    /// The tasks under the root on disk, by id.
    existing: HashMap<&'a str, &'a Task>,
    /// Whether the root resolved to a task, so ids can be checked against `existing`.
    have_root: bool,
    /// The line of the first task line carrying each id.
    ids_seen: HashMap<String, usize>,
    /// Every label anywhere in the proposal.
    labels: HashSet<String>,
}

/// One task of a sibling set, as it will be after the proposal.
struct Member {
    name: String,
    group: bool,
    stackable: bool,
    repository: Option<String>,
}

fn error(line: usize, message: impl Into<String>) -> Problem {
    Problem::error(line, message)
}

fn warning(line: usize, message: impl Into<String>) -> Problem {
    Problem {
        line,
        severity: Severity::Warning,
        message: message.into(),
    }
}

fn edge_word(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::After => "after",
        EdgeKind::StackedOn => "stacked on",
    }
}

fn is_pr_kind(kind: Kind) -> bool {
    matches!(kind, Kind::PrShip | Kind::PrMerge)
}

impl<'a> Validator<'a> {
    fn run(&mut self, parsed: &'a Parsed) {
        let root = &parsed.root;
        let file_task = self
            .checks
            .file
            .as_ref()
            .map(|file| file.task_id.as_str())
            .or_else(|| {
                parsed
                    .header
                    .for_task
                    .as_ref()
                    .map(|task| task.value.as_str())
            });

        if let (Some(file), Some(number)) = (&self.checks.file, &parsed.header.number)
            && file.number != number.value
        {
            self.problems.push(error(
                number.line,
                format!(
                    "proposal {} does not match the file name: this file is {}.proposal.txt",
                    number.value, file.number
                ),
            ));
        }
        if let (Some(file), Some(task)) = (&self.checks.file, &parsed.header.for_task)
            && file.task_id != task.value
        {
            self.problems.push(error(
                task.line,
                format!(
                    "'for {}' is not the task this file belongs to ({})",
                    task.value, file.task_id
                ),
            ));
        }

        if root.line == 0 {
            // The parser found no task and said so.
            return;
        }
        collect_labels(root, &mut self.labels);

        let mut root_task = None;
        match (&root.attributes.id, file_task) {
            (None, Some(expected)) => self.problems.push(error(
                root.line,
                format!("the root task needs '| id {expected}': it is the task this file is for"),
            )),
            (None, None) => self.problems.push(error(
                root.line,
                "the root task needs an '| id' naming the task this file is for",
            )),
            (Some(id), expected) => {
                if let Some(expected) = expected
                    && expected != id
                {
                    self.problems.push(error(
                        root.line,
                        format!("the root's id {id} is not the task this file is for ({expected})"),
                    ));
                }
                root_task = self
                    .tree
                    .tasks()
                    .into_iter()
                    .find(|task| task.id() == Some(id.as_str()));
            }
        }

        let mut repository = None;
        if let Some(task) = root_task {
            self.have_root = true;
            for task in task.walk() {
                if let Some(id) = task.id() {
                    self.existing.insert(id, task);
                }
            }
            if let Some(file) = &task.file
                && let Some(id) = task.id()
                && let Ok(place) = Place::locate(self.tree, id)
            {
                repository = place.repository(file);
            }
        }
        self.visit(root, None, root_task, repository);
    }

    /// Checks one task line and everything under it. `parent` is the proposal's parent node and
    /// its task on disk; `disk` this node's task on disk; `repository` the repository this node
    /// is in, which its children inherit.
    fn visit(
        &mut self,
        node: &'a Node,
        parent: Option<(&'a Node, Option<&'a Task>)>,
        disk: Option<&'a Task>,
        repository: Option<String>,
    ) {
        self.check_identity(node, parent.is_some());
        self.check_keeps_work(node, parent);
        let group = !node.children.is_empty();
        if group {
            for result in &node.results {
                self.problems.push(error(result.line, GROUP_HAS_NO_WORK));
            }
            for todo in &node.todos {
                self.problems.push(error(todo.line, GROUP_HAS_NO_WORK));
            }
        } else if let Some(second) = node.results.get(1) {
            self.problems
                .push(error(second.line, "two results, split or merge"));
        }
        self.check_todos(node, group);
        if !group && node.results.is_empty() && !disk.is_some_and(|task| self.is_closed(task)) {
            self.problems.push(warning(node.line, "no result named"));
        }
        if group {
            self.check_siblings(node, disk, &repository);
        }
        for child in &node.children {
            let child_disk = self.disk_of(child);
            let child_repository = child_disk
                .and_then(|task| task.file.as_ref()?.repository.clone())
                .or_else(|| repository.clone());
            self.visit(child, Some((node, disk)), child_disk, child_repository);
        }
    }

    fn disk_of(&self, node: &Node) -> Option<&'a Task> {
        self.existing.get(node.attributes.id.as_deref()?).copied()
    }

    fn is_closed(&self, task: &Task) -> bool {
        matches!(
            derive::state(task, self.tree),
            State::Done | State::Cancelled
        )
    }

    fn check_identity(&mut self, node: &Node, has_parent: bool) {
        let Some(id) = &node.attributes.id else {
            return;
        };
        if let Some(first) = self.ids_seen.get(id) {
            self.problems.push(error(
                node.line,
                format!("duplicate id '{id}': also on line {first}"),
            ));
            return;
        }
        self.ids_seen.insert(id.clone(), node.line);
        // The root's id is checked against the file's task; every other id must be under it.
        if has_parent && self.have_root && !self.existing.contains_key(id.as_str()) {
            self.problems.push(error(
                node.line,
                format!("unknown id '{id}': no task under the root has it"),
            ));
        }
    }

    fn check_keeps_work(&mut self, node: &Node, parent: Option<(&Node, Option<&Task>)>) {
        if !node.attributes.keeps_work {
            return;
        }
        let Some((parent_node, parent_disk)) = parent else {
            self.problems.push(error(
                node.line,
                "'keeps work' is for a child of a leaf being split, and the root has no parent",
            ));
            return;
        };
        let splits_a_leaf = parent_disk.is_some_and(|task| {
            task.children.is_empty() && derive::shape(task, self.tree) == derive::Shape::Leaf
        });
        if !splits_a_leaf {
            self.problems.push(error(
                node.line,
                format!(
                    "'keeps work' is for a child of a leaf being split; '{}' is not an existing \
                     leaf",
                    parent_node.label
                ),
            ));
            return;
        }
        let first = parent_node
            .children
            .iter()
            .find(|child| child.attributes.keeps_work)
            .map_or(node.line, |child| child.line);
        if first != node.line {
            self.problems.push(error(
                node.line,
                format!(
                    "a second 'keeps work' under '{}': one child keeps the work, marked on line \
                     {first}",
                    parent_node.label
                ),
            ));
        }
    }

    fn check_todos(&mut self, node: &Node, group: bool) {
        let mut built = Vec::new();
        for todo in &node.todos {
            if !group
                && let Some(role) = todo.role
                && let Err(message) = check_role_free(&node.label, &built, None, role)
            {
                self.problems.push(error(todo.line, message));
            }
            built.push(bongos_work_model::Todo {
                name: todo.name.clone(),
                role: todo.role,
                workflow: None,
                state: TodoState::Open,
                started: None,
                extra: Extra::new(),
            });
            let Some(workflow) = &todo.workflow else {
                continue;
            };
            match &self.checks.workflows {
                None => self.problems.push(warning(
                    todo.line,
                    format!("workflow '{workflow}' is not checked: no workflow engine yet"),
                )),
                Some(known) if !known.contains(workflow) => {
                    let list: Vec<&str> = known.iter().map(String::as_str).collect();
                    let message = if list.is_empty() {
                        format!("unknown workflow '{workflow}': no workflows are registered")
                    } else {
                        format!(
                            "unknown workflow '{workflow}': the known workflows are {}",
                            list.join(", ")
                        )
                    };
                    self.problems.push(error(todo.line, message));
                }
                Some(_) => {}
            }
        }
        if !group && node.todos.len() > self.checks.todo_warning {
            self.problems.push(warning(
                node.line,
                format!("{} todos; is this more than one result?", node.todos.len()),
            ));
        }
    }

    /// The members of the sibling set under `parent`: the proposed children, then the children on
    /// disk the proposal does not mention.
    fn members(
        &self,
        parent: &Node,
        parent_disk: Option<&'a Task>,
        repository: &Option<String>,
    ) -> (Vec<Member>, Vec<&'a Task>) {
        let mut members = Vec::new();
        let mut mentioned = HashSet::new();
        for child in &parent.children {
            let disk = self.disk_of(child);
            if let Some(task) = disk {
                mentioned.insert(task.dir.clone());
            }
            let group = !child.children.is_empty();
            let own = disk
                .and_then(|task| task.file.as_ref()?.repository.clone())
                .or_else(|| repository.clone());
            let stackable = !group && file_stackable(&self.proposed_file(child, disk));
            members.push(Member {
                name: child.label.clone(),
                group,
                stackable,
                repository: own,
            });
        }
        let mut untouched = Vec::new();
        for task in parent_disk.map_or(&[][..], |task| task.children.as_slice()) {
            if mentioned.contains(&task.dir) {
                continue;
            }
            let Some(file) = &task.file else { continue };
            members.push(Member {
                name: task.slug.clone(),
                group: !task.children.is_empty(),
                stackable: file_stackable(file),
                repository: file.repository.clone().or_else(|| repository.clone()),
            });
            untouched.push(task);
        }
        (members, untouched)
    }

    /// A stand-in for the file a proposed leaf has after the apply, enough to ask whether it is
    /// stackable: its todos with roles. A new PR-kind leaf whose todos carry no role gets
    /// `produces-branch` on `implement` and `ships` on `ship` or `merge`, as at creation; a leaf
    /// with no todo lines keeps what is on disk, or the kind's defaults when new.
    fn proposed_file(&self, node: &Node, disk: Option<&Task>) -> TaskFile {
        let kind = node
            .attributes
            .kind
            .or_else(|| disk.and_then(|task| Some(task.file.as_ref()?.kind)))
            .unwrap_or(Kind::PrShip);
        let mut file = TaskFile::new(node.goal.clone(), kind);
        file.todos = if !node.todos.is_empty() {
            let mut todos: Vec<_> = node
                .todos
                .iter()
                .map(|todo| bongos_work_model::Todo {
                    name: todo.name.clone(),
                    role: todo.role,
                    workflow: None,
                    state: TodoState::Open,
                    started: None,
                    extra: Extra::new(),
                })
                .collect();
            if disk.is_none() && is_pr_kind(kind) && todos.iter().all(|todo| todo.role.is_none()) {
                for (names, role) in [
                    (&["implement"][..], TodoRole::ProducesBranch),
                    (&["ship", "merge"][..], TodoRole::Ships),
                ] {
                    if let Some(todo) = todos
                        .iter_mut()
                        .find(|todo| names.contains(&todo.name.as_str()))
                    {
                        todo.role = Some(role);
                    }
                }
            }
            todos
        } else if let Some(existing) = disk.and_then(|task| task.file.as_ref()) {
            existing.todos.clone()
        } else {
            default_todos(kind)
        };
        file
    }

    /// Duplicate labels, then every edge of the children, in file order, against a graph that
    /// already holds the edges that stay on disk.
    fn check_siblings(
        &mut self,
        parent: &'a Node,
        parent_disk: Option<&'a Task>,
        repository: &Option<String>,
    ) {
        let mut first_label: HashMap<&str, usize> = HashMap::new();
        for child in &parent.children {
            match first_label.get(child.label.as_str()) {
                Some(first) => self.problems.push(error(
                    child.line,
                    format!(
                        "duplicate label '{}' among siblings: also on line {first}",
                        child.label
                    ),
                )),
                None => {
                    first_label.insert(child.label.as_str(), child.line);
                }
            }
        }

        let (members, untouched) = self.members(parent, parent_disk, repository);

        // The slug an on-disk edge names, as this proposal calls that task.
        let mut label_of_slug: HashMap<&str, &str> = HashMap::new();
        for child in &parent.children {
            if let Some(task) = self.disk_of(child) {
                label_of_slug.insert(task.slug.as_str(), child.label.as_str());
            }
        }
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        let mut kept: Vec<&Task> = untouched.clone();
        for child in &parent.children {
            if let Some(task) = self.disk_of(child)
                && self.is_closed(task)
            {
                kept.push(task);
            }
        }
        for task in kept {
            let Some(file) = &task.file else { continue };
            let from = label_of_slug
                .get(task.slug.as_str())
                .copied()
                .unwrap_or(task.slug.as_str());
            let targets = file
                .after
                .iter()
                .map(|edge| edge.task.as_str())
                .chain(file.stacked_on.iter().map(|edge| edge.task.as_str()));
            for target in targets {
                let target = label_of_slug.get(target).copied().unwrap_or(target);
                graph
                    .entry(from.to_string())
                    .or_default()
                    .push(target.to_string());
            }
        }

        // A task that becomes a group cannot stay at the end of a stack kept on disk.
        for child in parent.children.iter().filter(|c| !c.children.is_empty()) {
            let slug = self
                .disk_of(child)
                .map_or(child.label.as_str(), |task| task.slug.as_str());
            for task in &untouched {
                let stacked = task
                    .file
                    .as_ref()
                    .and_then(|file| file.stacked_on.as_ref())
                    .is_some_and(|edge| edge.task == slug);
                if stacked {
                    self.problems.push(error(
                        child.line,
                        format!(
                            "'{} stacked on {}': {}",
                            task.slug,
                            child.label,
                            group_stacked(&task.slug, &child.label)
                        ),
                    ));
                }
            }
        }

        for child in &parent.children {
            if self.disk_of(child).is_some_and(|task| self.is_closed(task)) {
                // Edges on a done or cancelled task stay as they are.
                continue;
            }
            let mut targets_seen: HashSet<&str> = HashSet::new();
            let mut stacked_line: Option<usize> = None;
            for edge in &child.edges {
                let Some(target) = self.resolve_edge(child, edge, &members) else {
                    continue;
                };
                let text = edge_text(child, edge);
                if let Some(message) =
                    self.edge_refusal(child, edge, &members, target, &targets_seen, stacked_line)
                {
                    self.problems
                        .push(error(edge.line, format!("'{text}': {message}")));
                    continue;
                }
                targets_seen.insert(target);
                if edge.kind == EdgeKind::StackedOn {
                    stacked_line = Some(edge.line);
                }
                if let Some(path) = find_cycle(&graph, &child.label, target) {
                    self.problems.push(error(
                        edge.line,
                        format!("cycle: '{text}' closes {}", path.join(" -> ")),
                    ));
                    continue;
                }
                graph
                    .entry(child.label.clone())
                    .or_default()
                    .push(target.to_string());
            }
        }
    }

    /// The label an edge points at, or a problem when it does not name a sibling.
    fn resolve_edge<'e>(
        &mut self,
        child: &Node,
        edge: &'e Edge,
        members: &[Member],
    ) -> Option<&'e str> {
        let text = edge_text(child, edge);
        if edge.target.len() > 1 {
            self.problems.push(error(
                edge.line,
                format!("cross-level: '{text}' joins siblings only"),
            ));
            return None;
        }
        let name = edge.target.first()?;
        if find_member(members, name).is_some() {
            return Some(name);
        }
        let message = if self.labels.contains(name) {
            format!("cross-level: '{text}' joins siblings only")
        } else {
            format!("unknown task '{name}' in '{text}'")
        };
        self.problems.push(error(edge.line, message));
        None
    }

    /// Feature 03's refusals for a new edge (`check_edge`), over the proposed siblings, in its
    /// order and words. The cycle is checked by the caller.
    fn edge_refusal(
        &self,
        child: &Node,
        edge: &Edge,
        members: &[Member],
        target: &str,
        seen: &HashSet<&str>,
        stacked_line: Option<usize>,
    ) -> Option<String> {
        let slug = child.label.as_str();
        if target == slug {
            return Some(format!("{slug} cannot have an edge to itself"));
        }
        if seen.contains(target) {
            return Some(format!("{slug} already has an edge to {target}"));
        }
        if edge.kind != EdgeKind::StackedOn {
            return None;
        }
        if stacked_line.is_some() {
            let existing = child
                .edges
                .iter()
                .find(|other| other.kind == EdgeKind::StackedOn && other.line != edge.line)
                .map_or("another task", |other| {
                    other.target.first().map_or("another task", String::as_str)
                });
            return Some(format!(
                "{slug} is already stacked on {existing}; a task has at most one stacked on"
            ));
        }
        let own = find_member(members, slug)?;
        let other = find_member(members, target)?;
        if own.group || other.group {
            return Some(group_stacked(slug, target));
        }
        for (name, stackable) in [(slug, own.stackable), (target, other.stackable)] {
            if !stackable {
                return Some(format!(
                    "stacked on needs both tasks stackable; {name} needs a produces-branch todo \
                     and a ships todo"
                ));
            }
        }
        if own.repository != other.repository {
            let name = |repository: &Option<String>| {
                repository
                    .clone()
                    .unwrap_or_else(|| "no repository".to_string())
            };
            return Some(format!(
                "stacked on needs both tasks in one repository; {slug} is in {}, {target} in {}",
                name(&own.repository),
                name(&other.repository)
            ));
        }
        None
    }
}

fn find_member<'m>(members: &'m [Member], name: &str) -> Option<&'m Member> {
    members.iter().find(|member| member.name == name)
}

const GROUP_HAS_NO_WORK: &str = "a group has no todos or result; move them to a leaf";

fn group_stacked(slug: &str, target: &str) -> String {
    format!("{slug} is stacked on {target}, and a group cannot be stacked; remove that edge first")
}

fn edge_text(child: &Node, edge: &Edge) -> String {
    format!(
        "{} {} {}",
        child.label,
        edge_word(edge.kind),
        edge.target.join("/")
    )
}

fn collect_labels(node: &Node, labels: &mut HashSet<String>) {
    labels.insert(node.label.clone());
    for child in &node.children {
        collect_labels(child, labels);
    }
}

/// The chain closed by a new edge `from -> target` over `graph`, starting at the target and
/// ending where it began, such as `integration -> api -> integration`: the walk of feature 03's
/// `rules::cycle`, run on the proposed sibling set.
fn find_cycle(
    graph: &HashMap<String, Vec<String>>,
    from: &str,
    target: &str,
) -> Option<Vec<String>> {
    fn walk(
        node: &str,
        goal: &str,
        graph: &HashMap<String, Vec<String>>,
        path: &mut Vec<String>,
        seen: &mut HashSet<String>,
    ) -> bool {
        path.push(node.to_string());
        if node == goal {
            return true;
        }
        if seen.insert(node.to_string()) {
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
    let mut seen = HashSet::new();
    if !walk(target, from, graph, &mut path, &mut seen) {
        return None;
    }
    // `path` runs target ... from; close it on the target.
    path.push(target.to_string());
    Some(path)
}
