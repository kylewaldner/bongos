use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bongos_work_model::{Status, TodoState};
use tempfile::TempDir;

use crate::{DefinitionTree, RelPath, TaskStateRecord, TodoStateRecord, work_tree_from};

const PROJECT: &str = "schema = 1\nid = \"p1\"\ntitle = \"Invoicing\"\n";

fn task(id: &str, goal: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n\
         created = 2026-01-01T10:00:00Z\n\n[[todos]]\nid = \"{id}-t1\"\nname = \"implement\"\n"
    )
}

fn files() -> Vec<(&'static str, String)> {
    vec![
        ("project.toml", PROJECT.to_string()),
        ("task.toml", task("g1", "Invoicing")),
        ("plan.md", "# plan\n".to_string()),
        ("documents/notes.md", "notes\n".to_string()),
        ("tasks/backfill/task.toml", task("l1", "Backfill")),
        ("tasks/backfill/documents/a.md", "a\n".to_string()),
        ("tasks/backfill/tasks/inner/task.toml", task("l2", "Inner")),
        ("tasks/report/task.toml", task("l3", "Report")),
    ]
}

fn write_dir(root: &Path) {
    for (path, text) in files() {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn blobs() -> Vec<(RelPath, Vec<u8>)> {
    files()
        .into_iter()
        .map(|(path, text)| (RelPath::new(path).unwrap(), text.into_bytes()))
        .collect()
}

#[test]
fn parse_dir_and_parse_files_of_the_same_tree_are_equal() {
    let temp = TempDir::new().unwrap();
    write_dir(temp.path());
    // A `.git` inside definitions is never read.
    std::fs::create_dir_all(temp.path().join(".git")).unwrap();
    std::fs::write(temp.path().join(".git/task.toml"), "not toml [").unwrap();

    let from_dir = DefinitionTree::parse_dir(temp.path()).unwrap();
    let from_files = DefinitionTree::parse_files(blobs()).unwrap();
    assert_eq!(from_dir, from_files);

    let folders: Vec<(&str, &str)> = from_dir
        .nodes()
        .into_iter()
        .map(|node| (node.folder.as_str(), node.definition.id.as_str()))
        .collect();
    assert_eq!(
        folders,
        [
            ("", "g1"),
            ("tasks/backfill", "l1"),
            ("tasks/backfill/tasks/inner", "l2"),
            ("tasks/report", "l3"),
        ]
    );
    assert_eq!(from_dir.project.unwrap().title, "Invoicing");
}

#[test]
fn problems_name_the_file() {
    let mut files = blobs();
    files.push((
        RelPath::new("tasks/broken/task.toml").unwrap(),
        b"schema = 1\nid = ".to_vec(),
    ));
    files.push((
        RelPath::new("tasks/stateful/task.toml").unwrap(),
        b"schema = 1\nid = \"s1\"\ngoal = \"Stateful\"\nkind = \"other\"\n\
          created = 2026-01-01T10:00:00Z\nstatus = \"done\"\n"
            .to_vec(),
    ));
    files.push((
        RelPath::new("tasks/orphan/tasks/child/task.toml").unwrap(),
        task("c1", "Child").into_bytes(),
    ));
    let problems = DefinitionTree::parse_files(files).unwrap_err();
    let paths: Vec<&str> = problems.iter().map(|problem| problem.path.as_str()).collect();
    assert!(paths.contains(&"tasks/broken/task.toml"), "{problems:?}");
    assert!(paths.contains(&"tasks/stateful/task.toml"), "{problems:?}");
    assert!(paths.contains(&"tasks/orphan/task.toml"), "{problems:?}");

    let missing = DefinitionTree::parse_files(Vec::new()).unwrap_err();
    assert_eq!(missing.len(), 2);
}

#[test]
fn rel_paths_stay_inside_definitions() {
    assert!(RelPath::new("tasks/a/task.toml").is_some());
    for bad in ["", "/abs", "a/../b", "a//b", "./a"] {
        assert!(RelPath::new(bad).is_none(), "{bad}");
    }
}

fn state() -> BTreeMap<String, TaskStateRecord> {
    let mut state = BTreeMap::new();
    state.insert(
        "l1".to_string(),
        TaskStateRecord {
            status: Status::Done,
            todos: vec![TodoStateRecord {
                id: Some("l1-t1".to_string()),
                state: TodoState::Done,
                started: None,
            }],
            ..TaskStateRecord::default()
        },
    );
    state
}

#[test]
fn work_tree_from_joins_definitions_with_state_and_scans_files_folders() {
    let temp = TempDir::new().unwrap();
    let files_root = temp.path().join("files");
    let backfill = files_root.join("l1");
    std::fs::create_dir_all(backfill.join("chats")).unwrap();
    std::fs::write(backfill.join("chats/one.md"), "hi").unwrap();
    std::fs::write(backfill.join("prompts.txt"), "").unwrap();
    // A repository checked out in a files folder is never an artifact.
    std::fs::create_dir_all(backfill.join(".git/objects")).unwrap();
    std::fs::write(backfill.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let definitions = DefinitionTree::parse_files(blobs()).unwrap();
    let tree = work_tree_from(&definitions, &state(), &files_root);

    assert_eq!(tree.root, files_root);
    assert_eq!(tree.projects.len(), 1);
    let project = &tree.projects[0];
    assert_eq!(project.slug, "invoicing");
    assert_eq!(project.task.dir, files_root.join("g1"));
    assert_eq!(project.task.definition, Some(PathBuf::new()));

    let task = tree.task_by_id("l1").unwrap();
    assert_eq!(task.dir, backfill);
    assert_eq!(task.definition, Some(PathBuf::from("tasks/backfill")));
    let file = task.file.as_ref().unwrap();
    assert_eq!(file.status, Status::Done);
    assert_eq!(file.todos[0].state, TodoState::Done);
    let names: Vec<&Path> = task.files.keys().map(PathBuf::as_path).collect();
    assert_eq!(
        names,
        [Path::new("chats/one.md"), Path::new("prompts.txt")]
    );

    // A task with no state row reads as open with open todos, and no files folder is fine.
    let inner = tree.task_by_id("l2").unwrap();
    assert_eq!(inner.file.as_ref().unwrap().status, Status::Open);
    assert_eq!(inner.file.as_ref().unwrap().todos[0].state, TodoState::Open);
    assert!(inner.files.is_empty());
    assert_eq!(tree.parent_of("l2").unwrap().id(), Some("l1"));
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
}

#[test]
fn an_empty_definition_tree_yields_no_project() {
    let temp = TempDir::new().unwrap();
    let tree = work_tree_from(&DefinitionTree::default(), &state(), temp.path());
    assert!(tree.projects.is_empty());
}

#[test]
fn differing_ids_names_changed_added_and_removed_tasks() {
    let before = DefinitionTree::parse_files(blobs()).unwrap();
    let mut changed = blobs();
    changed.retain(|(path, _)| path.as_str() != "tasks/report/task.toml");
    for (path, bytes) in &mut changed {
        if path.as_str() == "tasks/backfill/task.toml" {
            *bytes = task("l1", "Backfill renamed").into_bytes();
        }
    }
    changed.push((
        RelPath::new("tasks/new/task.toml").unwrap(),
        task("n1", "New").into_bytes(),
    ));
    let after = DefinitionTree::parse_files(changed).unwrap();
    let ids: Vec<String> = before.differing_ids(&after).into_iter().collect();
    assert_eq!(ids, ["l1", "l3", "n1"]);
}
