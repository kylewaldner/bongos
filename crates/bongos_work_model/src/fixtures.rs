//! In-memory trees for the roll-up, activity, graph, rules and command tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::commands::{Change, Context, Write};
use crate::store::{Activity, FileError, Project, Task, WorkTree};
use crate::*;

pub fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

pub fn todo(name: &str, role: Option<TodoRole>) -> Todo {
    Todo {
        id: None,
        name: name.to_string(),
        role,
        workflow: None,
        state: TodoState::Open,
        started: None,
        extra: Extra::new(),
    }
}

/// A `pr-ship` leaf with todos implement, review and ship, none started.
pub fn pr_ship(slug: &str) -> Task {
    let mut file = TaskFile::new(slug, Kind::PrShip);
    file.todos = vec![
        todo("implement", Some(TodoRole::ProducesBranch)),
        todo("review", None),
        todo("ship", Some(TodoRole::Ships)),
    ];
    task(slug, file)
}

/// An `other` leaf with the given status.
pub fn leaf(slug: &str, status: Status) -> Task {
    let mut file = TaskFile::new(slug, Kind::Other);
    file.status = status;
    task(slug, file)
}

pub fn task(slug: &str, file: TaskFile) -> Task {
    Task {
        slug: slug.to_string(),
        dir: PathBuf::from(slug),
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: BTreeMap::new(),
        hashes: BTreeMap::new(),
        children: Vec::new(),
    }
}

pub fn group(slug: &str, children: Vec<Task>) -> Task {
    let mut task = task(slug, TaskFile::new(slug, Kind::Other));
    task.children = children;
    task
}

pub fn file_mut(task: &mut Task) -> &mut TaskFile {
    task.file.as_mut().unwrap()
}

pub fn with_created(mut task: Task, created: &str) -> Task {
    file_mut(&mut task).created = at(created);
    task
}

pub fn after(mut task: Task, target: &str, unblock: UnblockPoint) -> Task {
    file_mut(&mut task).after.push(Edge {
        task: target.to_string(),
        unblock,
        extra: Extra::new(),
    });
    task
}

pub fn stacked_on(mut task: Task, target: &str, unblock: UnblockPoint) -> Task {
    file_mut(&mut task).stacked_on = Some(StackedOn {
        task: target.to_string(),
        unblock,
        extra: Extra::new(),
    });
    task
}

pub fn place(task: &mut Task, parent: &Path) {
    task.dir = parent.join(&task.slug);
    let dir = task.dir.clone();
    for child in &mut task.children {
        place(child, &dir.join("tasks"));
    }
}

pub fn project(slug: &str, children: Vec<Task>) -> Project {
    let mut top = group(slug, children);
    place(&mut top, Path::new("/w/projects"));
    Project {
        slug: slug.to_string(),
        file: Some(ProjectFile::new(slug)),
        task: top,
    }
}

pub fn activity(slug: &str, mut oneoffs: Vec<Task>) -> Activity {
    let dir = Path::new("/w/oneoffs").join(slug);
    for oneoff in &mut oneoffs {
        place(oneoff, &dir);
    }
    Activity {
        slug: slug.to_string(),
        dir,
        oneoffs,
    }
}

/// A one-project tree whose top group `p` holds `children`.
pub fn tree(children: Vec<Task>) -> WorkTree {
    WorkTree {
        root: PathBuf::from("/w"),
        projects: vec![project("p", children)],
        ..WorkTree::default()
    }
}

pub fn find<'a>(tree: &'a WorkTree, slug: &str) -> &'a Task {
    tree.tasks()
        .into_iter()
        .find(|task| task.slug == slug)
        .unwrap()
}

pub fn id_of(tree: &WorkTree, slug: &str) -> TaskId {
    find(tree, slug).id().unwrap().to_string()
}

/// Kyle at a fixed time.
pub fn ctx() -> Context {
    Context::new("person", at("2026-11-08T12:00:00Z"))
}

/// Applies the task writes of `change` to the tasks already in `tree`, in memory.
pub fn update(tree: &mut WorkTree, change: &Change) {
    fn visit(task: &mut Task, change: &Change) {
        for write in &change.writes {
            if let Write::Task { dir, file } = write
                && *dir == task.dir
            {
                task.file = Some(file.clone());
            }
        }
        for child in &mut task.children {
            visit(child, change);
        }
    }
    for project in &mut tree.projects {
        visit(&mut project.task, change);
    }
    for activity in &mut tree.activities {
        for oneoff in &mut activity.oneoffs {
            visit(oneoff, change);
        }
    }
}

/// `task` with an error badge on its `task.toml`, as the loader leaves an invalid file.
pub fn badged(mut task: Task) -> Task {
    task.errors.push(FileError {
        path: task.dir.join("task.toml"),
        line: Some(3),
        reason: "expected a value".to_string(),
    });
    task
}

/// Marks the todos of `task` at `indices` done.
pub fn done(mut task: Task, indices: &[usize]) -> Task {
    for index in indices {
        file_mut(&mut task).todos[*index].state = TodoState::Done;
    }
    task
}
