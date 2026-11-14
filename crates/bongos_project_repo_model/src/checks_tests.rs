use crate::RelPath;
use crate::checks::{layout_problems, schema_problems};

const PROJECT: &str = "schema = 1\nid = \"p\"\ntitle = \"P\"\n";
const TOP: &str =
    "schema = 1\nid = \"g1\"\ngoal = \"P\"\nkind = \"other\"\ncreated = 2026-01-01T10:00:00Z\n";

#[test]
fn only_known_paths_pass_the_layout() {
    let paths = [
        ".gitattributes",
        "definitions/project.toml",
        "definitions/task.toml",
        "definitions/plan.md",
        "definitions/tasks/a/task.toml",
        "definitions/documents/notes.md",
        "definitions/stamp.txt",
        "definitions/junk.txt",
        "state/state.db",
    ];
    let problems = layout_problems(paths, &["stamp.txt".to_string()]);
    let named: Vec<&str> = problems.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(named, ["junk.txt", "state/state.db"]);
}

#[test]
fn the_schema_check_reports_unparsable_definitions() {
    let files = vec![
        (
            RelPath::new("project.toml").unwrap(),
            PROJECT.as_bytes().to_vec(),
        ),
        (RelPath::new("task.toml").unwrap(), TOP.as_bytes().to_vec()),
    ];
    assert!(schema_problems(["definitions/task.toml"], files.clone(), &[]).is_empty());
    let broken = vec![
        files[0].clone(),
        (RelPath::new("task.toml").unwrap(), b"not = [toml".to_vec()),
    ];
    let problems = schema_problems(["definitions/task.toml"], broken, &[]);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].path, "task.toml");
}
