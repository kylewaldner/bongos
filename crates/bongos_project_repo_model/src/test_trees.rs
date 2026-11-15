//! In-memory definition trees for the summary and check tests.

use crate::{DefinitionTree, RelPath};

pub const PROJECT: &str = "schema = 1\nid = \"p\"\ntitle = \"P\"\n";

/// The `task.toml` of a task with id and goal `id`, followed by `extra` lines.
pub fn task_toml(id: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{id}\"\nkind = \"other\"\ncreated = 2026-01-01T10:00:00Z\n{extra}"
    )
}

/// A tree of the top group (id `top`) and `tasks`, each `(folder, extra lines)`, whose id is the
/// folder's last part. The project file is [`PROJECT`].
pub fn tree(tasks: &[(&str, &str)]) -> DefinitionTree {
    tree_with_project(PROJECT, tasks)
}

pub fn tree_with_project(project: &str, tasks: &[(&str, &str)]) -> DefinitionTree {
    let mut files = vec![
        (
            RelPath::new("project.toml").unwrap(),
            project.as_bytes().to_vec(),
        ),
        (
            RelPath::new("task.toml").unwrap(),
            task_toml("top", "").into_bytes(),
        ),
    ];
    for (folder, extra) in tasks {
        let id = folder.rsplit('/').next().unwrap();
        files.push((
            RelPath::new(format!("{folder}/task.toml")).unwrap(),
            task_toml(id, extra).into_bytes(),
        ));
    }
    DefinitionTree::parse_files(files).expect("the fixture parses")
}

/// A tree whose tasks are given by their full `task.toml` text, `(folder, text)`.
pub fn tree_of_files(tasks: &[(&str, String)]) -> DefinitionTree {
    tree_of_files_with_project(PROJECT, tasks)
}

pub fn tree_of_files_with_project(project: &str, tasks: &[(&str, String)]) -> DefinitionTree {
    let mut files = vec![
        (
            RelPath::new("project.toml").unwrap(),
            project.as_bytes().to_vec(),
        ),
        (
            RelPath::new("task.toml").unwrap(),
            task_toml("top", "").into_bytes(),
        ),
    ];
    for (folder, text) in tasks {
        files.push((
            RelPath::new(format!("{folder}/task.toml")).unwrap(),
            text.clone().into_bytes(),
        ));
    }
    DefinitionTree::parse_files(files).expect("the fixture parses")
}

/// An `after` edge list naming `targets`.
pub fn after(targets: &[&str]) -> String {
    let edges: Vec<String> = targets
        .iter()
        .map(|target| format!("{{ task = \"{target}\", unblock = \"merged\" }}"))
        .collect();
    format!("after = [{}]\n", edges.join(", "))
}

/// A todo entry with `id` and `name`.
pub fn todo(id: &str, name: &str) -> String {
    format!("[[todos]]\nid = \"{id}\"\nname = \"{name}\"\n")
}
