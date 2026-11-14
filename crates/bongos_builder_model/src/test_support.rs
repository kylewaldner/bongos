//! Fixture trees for the tests: a work tree built in memory, with directories placed the way the
//! loader would place them.

use std::path::{Path, PathBuf};

use bongos_work_model::commands::default_todos;
use bongos_work_model::store::{Project, Task, WorkTree};
use bongos_work_model::{
    Edge, Extra, Kind, ProjectFile, StackedOn, TaskFile, Timestamp, Todo, TodoRole, TodoState,
    UnblockPoint,
};

pub fn timestamp(text: &str) -> Timestamp {
    text.parse().expect("a valid test timestamp")
}

fn task_with(slug: &str, goal: &str, kind: Kind, todos: Vec<Todo>) -> Task {
    let mut file = TaskFile::new(goal, kind);
    file.id = format!("id-{slug}");
    file.created = timestamp("2026-11-08T10:00:00Z");
    file.todos = todos;
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

/// A leaf with the default todos of its kind.
pub fn leaf(slug: &str, goal: &str, kind: Kind) -> Task {
    task_with(slug, goal, kind, default_todos(kind))
}

/// A leaf with exactly `todos`.
pub fn leaf_with(slug: &str, goal: &str, kind: Kind, todos: Vec<Todo>) -> Task {
    task_with(slug, goal, kind, todos)
}

pub fn group(slug: &str, goal: &str, children: Vec<Task>) -> Task {
    let mut task = task_with(slug, goal, Kind::Other, Vec::new());
    task.children = children;
    task
}

pub fn todo(name: &str, role: Option<TodoRole>, state: TodoState) -> Todo {
    Todo {
        id: None,
        name: name.to_string(),
        role,
        workflow: None,
        state,
        started: None,
        extra: Extra::new(),
    }
}

pub fn file_mut(task: &mut Task) -> &mut TaskFile {
    task.file.as_mut().expect("a task with a file")
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

pub fn planned(mut task: Task, result: &str) -> Task {
    file_mut(&mut task).planned_result = Some(result.to_string());
    task
}

pub fn finished(mut task: Task) -> Task {
    for todo in &mut file_mut(&mut task).todos {
        todo.state = TodoState::Done;
    }
    task
}

fn place(task: &mut Task, parent: &Path) {
    task.dir = parent.join(&task.slug);
    let children_dir = task.dir.join("tasks");
    for child in &mut task.children {
        place(child, &children_dir);
    }
}

/// A tree whose one project's top group is `top`.
pub fn tree(mut top: Task) -> WorkTree {
    place(&mut top, Path::new("/w/projects"));
    WorkTree {
        root: PathBuf::from("/w"),
        projects: vec![Project {
            slug: top.slug.clone(),
            file: Some(ProjectFile::new(top.slug.clone())),
            task: top,
        }],
        ..WorkTree::default()
    }
}

pub fn id_of(slug: &str) -> String {
    format!("id-{slug}")
}
