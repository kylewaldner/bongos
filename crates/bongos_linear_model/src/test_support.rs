//! Fixtures for the tests: work trees built in memory and written to a temporary directory, and
//! tickets.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use bongos_work_model::commands::{Change, Context, apply, default_todos};
use bongos_work_model::store::{Project, Task, WorkTree, load};
use bongos_work_model::{
    Extra, Kind, LinearState, ProjectFile, ProposalRecord, ProposalState, StackedOn, TaskFile,
    Timestamp, TodoRole, UnblockPoint, layout, serialize_project, serialize_task,
};
use futures::executor::block_on;
use tempfile::TempDir;

use crate::api::{Issue, IssueRef, LinearApi, StateKind, WorkflowState};
use crate::fake::FakeLinear;
use crate::ident::parse_identifier;

pub fn timestamp(text: &str) -> Timestamp {
    text.parse().expect("a valid test timestamp")
}

pub fn ctx() -> Context {
    Context::new("kyle", timestamp("2026-11-09T12:00:00Z"))
}

pub fn task_id(slug: &str) -> String {
    format!("id-{slug}")
}

fn make(slug: &str, goal: &str, kind: Kind, with_todos: bool) -> Task {
    let mut file = TaskFile::new(goal, kind);
    file.id = task_id(slug);
    file.created = timestamp("2026-11-08T10:00:00Z");
    if with_todos {
        file.todos = default_todos(kind);
    }
    Task {
        slug: slug.to_string(),
        dir: PathBuf::new(),
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        definition: None,
        pending: None,
        needs_conversion: false,
        broken_edges: Vec::new(),
        files: Default::default(),
        hashes: Default::default(),
        children: Vec::new(),
    }
}

/// A `pr-ship` leaf whose goal is its slug.
pub fn leaf(slug: &str) -> Task {
    make(slug, slug, Kind::PrShip, true)
}

/// A leaf with one `plan` todo (role `plans`), like a leaf made from a ticket.
pub fn plan_leaf(slug: &str) -> Task {
    let mut task = make(slug, slug, Kind::PrShip, false);
    file_mut(&mut task).todos = vec![crate::sync::plan_todo()];
    task
}

pub fn group(slug: &str, children: Vec<Task>) -> Task {
    let mut task = make(slug, slug, Kind::Other, false);
    task.children = children;
    task
}

pub fn file_mut(task: &mut Task) -> &mut TaskFile {
    task.file.as_mut().expect("a task with a file")
}

pub fn goal(mut task: Task, goal: &str) -> Task {
    file_mut(&mut task).goal = goal.to_string();
    task
}

pub fn created(mut task: Task, at: &str) -> Task {
    file_mut(&mut task).created = timestamp(at);
    task
}

/// A synced link: `linear` and `linear_state`, with the issue id `uuid-<identifier>` and team
/// `team`.
pub fn synced(mut task: Task, identifier: &str) -> Task {
    let file = file_mut(&mut task);
    file.linear = Some(identifier.to_string());
    file.linear_state = Some(LinearState {
        issue_id: format!("uuid-{identifier}"),
        team_id: "team".to_string(),
        url: format!("https://linear.app/acme/issue/{identifier}"),
        description: String::new(),
        ..LinearState::default()
    });
    task
}

/// A hand-set `linear` and nothing else (feature 03's link).
pub fn hand_linked(mut task: Task, identifier: &str) -> Task {
    file_mut(&mut task).linear = Some(identifier.to_string());
    task
}

pub fn pending(mut task: Task, uuid: &str) -> Task {
    file_mut(&mut task).linear_state = Some(LinearState {
        pending_create: Some(uuid.to_string()),
        ..LinearState::default()
    });
    task
}

pub fn state_mut(task: &mut Task) -> &mut LinearState {
    file_mut(task)
        .linear_state
        .as_mut()
        .expect("a task with a link record")
}

pub fn stacked_on(mut task: Task, target: &str) -> Task {
    file_mut(&mut task).stacked_on = Some(StackedOn {
        task: target.to_string(),
        unblock: UnblockPoint::Merged,
        extra: Extra::new(),
    });
    task
}

pub fn applied(mut task: Task, number: u32, by: &str) -> Task {
    file_mut(&mut task).proposals.push(ProposalRecord {
        number,
        state: ProposalState::Applied,
        at: timestamp("2026-11-09T10:00:00Z"),
        by: by.to_string(),
        hash: format!("hash-{number}"),
        extra: Extra::new(),
    });
    task
}

/// Starts the `plans` todo of a leaf (or its first).
pub fn begin_plan(mut task: Task) -> Task {
    let at = timestamp("2026-11-09T11:00:00Z");
    let todos = &mut file_mut(&mut task).todos;
    let index = todos
        .iter()
        .position(|todo| todo.role == Some(TodoRole::Plans))
        .unwrap_or(0);
    todos[index].started = Some(at);
    task
}

pub fn cancelled(mut task: Task) -> Task {
    file_mut(&mut task).status = bongos_work_model::Status::Cancelled;
    task
}

fn place(task: &mut Task, dir: PathBuf) {
    task.dir = dir;
    let children = layout::children_dir(&task.dir);
    for child in &mut task.children {
        place(child, children.join(&child.slug));
    }
}

/// A work tree on disk with one project `p` whose top group holds `children`, and the tasks as
/// the loader reads them.
pub struct Disk {
    temp: TempDir,
}

impl Disk {
    pub fn new(children: Vec<Task>) -> Self {
        let temp = TempDir::new().expect("a temporary directory");
        let root = temp.path();
        let mut top = group("p", children);
        place(&mut top, layout::project_dir(root, "p"));
        let tree = WorkTree {
            root: root.to_path_buf(),
            projects: vec![Project {
                slug: "p".to_string(),
                file: Some(ProjectFile::new("p")),
                task: top,
            }],
            ..WorkTree::default()
        };
        materialize(&tree, root);
        Self { temp }
    }

    pub fn root(&self) -> &Path {
        self.temp.path()
    }

    pub fn tree(&self) -> WorkTree {
        let tree = load(self.root());
        for task in tree.tasks() {
            assert!(task.errors.is_empty(), "{}: {:?}", task.slug, task.errors);
        }
        assert!(tree.errors.is_empty(), "{:?}", tree.errors);
        tree
    }

    pub fn apply(&self, change: &Change) {
        apply(self.root(), change).expect("the change applies");
    }

    pub fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        snapshot(self.root())
    }
}

fn materialize(tree: &WorkTree, root: &Path) {
    for project in &tree.projects {
        let dir = layout::project_dir(root, &project.slug);
        fs::create_dir_all(&dir).expect("a project directory");
        let file = project.file.as_ref().expect("a project file");
        fs::write(
            layout::project_file(&dir),
            serialize_project(file).expect("a project file serializes"),
        )
        .expect("a project file is written");
    }
    for task in tree.tasks() {
        fs::create_dir_all(&task.dir).expect("a task directory");
        let file = task.file.as_ref().expect("a task file");
        fs::write(
            layout::task_file(&task.dir),
            serialize_task(file).expect("a task file serializes"),
        )
        .expect("a task file is written");
    }
}

/// Every file under `root`, journals included, with its content.
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("a readable directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                let relative = path.strip_prefix(root).expect("under the root");
                files.insert(relative.to_path_buf(), fs::read(&path).expect("a file"));
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

/// The paths whose bytes differ between two snapshots (added, removed or changed).
pub fn changed_paths(
    before: &BTreeMap<PathBuf, Vec<u8>>,
    after: &BTreeMap<PathBuf, Vec<u8>>,
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

pub fn task_toml(tree: &WorkTree, slug: &str) -> PathBuf {
    layout::task_file(&find(tree, slug).dir)
}

pub fn find<'a>(tree: &'a WorkTree, slug: &str) -> &'a Task {
    tree.tasks()
        .into_iter()
        .find(|task| task.slug == slug)
        .unwrap_or_else(|| panic!("no task {slug}"))
}

pub fn file<'a>(tree: &'a WorkTree, slug: &str) -> &'a TaskFile {
    find(tree, slug).file.as_ref().expect("a task file")
}

pub fn id(tree: &WorkTree, slug: &str) -> String {
    find(tree, slug).id().expect("a task id").to_string()
}

pub fn state(id: &str, kind: StateKind, position: f64) -> WorkflowState {
    WorkflowState {
        id: id.to_string(),
        name: id.to_string(),
        kind,
        position,
    }
}

/// The states of a team: two started states, so the lowest position is tested.
pub fn team_states() -> Vec<WorkflowState> {
    vec![
        state("triage", StateKind::Triage, 0.0),
        state("backlog", StateKind::Backlog, 1.0),
        state("todo", StateKind::Unstarted, 2.0),
        state("review", StateKind::Started, 5.0),
        state("doing", StateKind::Started, 3.0),
        state("done", StateKind::Completed, 6.0),
        state("dup", StateKind::Duplicate, 7.0),
        state("later", StateKind::Canceled, 9.0),
        state("canceled", StateKind::Canceled, 8.0),
    ]
}

pub fn ticket(number: u32, parent: Option<&Issue>) -> Issue {
    Issue {
        id: format!("uuid-SEQ-{number}"),
        identifier: format!("SEQ-{number}"),
        title: format!("Ticket {number}"),
        description: format!("Description {number}"),
        url: format!("https://linear.app/acme/issue/SEQ-{number}"),
        team_id: "team".to_string(),
        state: state("backlog", StateKind::Backlog, 1.0),
        parent: parent.map(Issue::summary),
        children: Vec::new(),
        children_truncated: false,
        blocks: Vec::new(),
    }
}

/// A fake workspace with team `team` and the tickets `1..=count`, where ticket 1 is the parent
/// of all the others.
pub fn fake_with_children(count: u32) -> FakeLinear {
    let fake = FakeLinear::new("acme");
    fake.add_team("team", "SEQ", team_states());
    let parent = ticket(1, None);
    fake.add_issue(parent.clone());
    for number in 2..=count + 1 {
        fake.add_issue(ticket(number, Some(&parent)));
    }
    fake
}

/// Reads `identifier` and its subtickets level by level, as the app does for an import, down to
/// `depth` levels, assembling `children` recursively.
pub fn read_levels(fake: &FakeLinear, identifier: &str, depth: usize) -> Issue {
    let reference = IssueRef::Identifier(parse_identifier(identifier).expect("an identifier"));
    let mut issue = block_on(fake.issues(vec![reference]))
        .expect("the fake answers")
        .into_iter()
        .next()
        .expect("the ticket exists");
    if depth > 0 {
        issue.children = issue
            .children
            .iter()
            .map(|child| read_levels(fake, &child.identifier, depth - 1))
            .collect();
    }
    issue
}
