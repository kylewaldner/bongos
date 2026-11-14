use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bongos_work_model::store::{Project, Task, WorkTree};
use bongos_work_model::{
    ArtifactsFile, Extra, Kind, ResultKind, TaskFile, TaskResult, Todo, TodoState,
};

use crate::definition::{Definition, parse};
use crate::inputs::{Input, InputValue, OVERRIDE_SOURCE, resolve, shared_artifact_paths};

fn definition(inputs: &str) -> Definition {
    let text = format!(
        "schema = 1\nname = \"d\"\nstart = \"a\"\n[inputs]\n{inputs}\n[nodes.a]\nkind = \"agent\"\nprompt = \"x\"\n"
    );
    parse(&text).unwrap()
}

fn task(dir: &str, file: Option<TaskFile>, files: &[&str], manifest: Option<&str>) -> Task {
    Task {
        slug: Path::new(dir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        dir: PathBuf::from(dir),
        file,
        artifacts: manifest.map(|text| toml::from_str::<ArtifactsFile>(text).unwrap()),
        errors: Vec::new(),
        definition: None,
        broken_edges: Vec::new(),
        files: files
            .iter()
            .map(|path| (PathBuf::from(path), SystemTime::UNIX_EPOCH))
            .collect(),
        hashes: BTreeMap::new(),
        children: Vec::new(),
    }
}

fn tree(top: Task) -> WorkTree {
    WorkTree {
        root: PathBuf::from("/root"),
        projects: vec![Project {
            slug: "p".to_string(),
            file: None,
            task: top,
        }],
        ..WorkTree::default()
    }
}

fn leaf_file() -> TaskFile {
    let mut file = TaskFile::new("Ship the parser", Kind::PrShip);
    file.branch = Some("feature/parser".to_string());
    file.base = Some("main".to_string());
    file.merge_is_ship = Some(true);
    file.result = Some(TaskResult {
        kind: ResultKind::Pr,
        url: Some("https://example.com/pr/1".to_string()),
        state: None,
        urls: Vec::new(),
        extra: Extra::new(),
    });
    file.todos = vec![Todo {
        id: None,
        name: "implement".to_string(),
        role: None,
        workflow: None,
        state: TodoState::Open,
        started: None,
        extra: Extra::new(),
    }];
    file
}

const MANIFEST: &str = r#"
[[artifact]]
path = "notes/design.md"
visibility = "shared"

[[artifact]]
path = "secret.md"
visibility = "human-only"

[[artifact]]
path = "plan.md"
visibility = "human-only"
"#;

const LEAF_DIR: &str = "/root/projects/p/tasks/leaf";

/// A project group with a shared plan, and a leaf under it whose own plan is human-only.
fn fixture() -> (WorkTree, Task) {
    let leaf = task(
        LEAF_DIR,
        Some(leaf_file()),
        &["task.toml", "notes/design.md", "secret.md", "plan.md"],
        Some(MANIFEST),
    );
    let mut top = task(
        "/root/projects/p",
        Some(TaskFile::new("The project", Kind::Other)),
        &["task.toml", "plan.md"],
        None,
    );
    top.children.push(leaf.clone());
    (tree(top), leaf)
}

fn value(inputs: &[Input], name: &str) -> Option<InputValue> {
    inputs
        .iter()
        .find(|input| input.name == name)
        .unwrap()
        .value
        .clone()
}

fn text(value: &str) -> Option<InputValue> {
    Some(InputValue::Text(value.to_string()))
}

#[test]
fn every_source_resolves() {
    let (tree, leaf) = fixture();
    let definition = definition(
        r#"goal = { type = "text", from = "task.goal" }
plan = { type = "path", from = "task.plan" }
artifacts = { type = "path", from = "task.artifacts" }
pr = { type = "text", from = "task.pr" }
prs = { type = "text", from = "task.prs" }
base = { type = "text", from = "task.base" }
branch = { type = "text", from = "task.branch" }
ship = { type = "text", from = "task.merge_is_ship" }
todo = { type = "text", from = "todo.name" }
style = { type = "text", from = "literal:be brief" }"#,
    );
    let inputs = resolve(&definition, &leaf, &tree, Some(0), &BTreeMap::new());
    assert!(inputs.iter().all(|input| !input.missing), "{inputs:?}");
    assert_eq!(value(&inputs, "goal"), text("Ship the parser"));
    // The leaf's own plan is human-only, so the group's shared plan is used.
    assert_eq!(value(&inputs, "plan"), text("/root/projects/p/plan.md"));
    assert_eq!(
        value(&inputs, "artifacts"),
        Some(InputValue::List(vec![format!(
            "{LEAF_DIR}/notes/design.md"
        )]))
    );
    assert_eq!(value(&inputs, "pr"), text("https://example.com/pr/1"));
    assert_eq!(
        value(&inputs, "prs"),
        Some(InputValue::List(vec![
            "https://example.com/pr/1".to_string()
        ]))
    );
    assert_eq!(value(&inputs, "base"), text("main"));
    assert_eq!(value(&inputs, "branch"), text("feature/parser"));
    assert_eq!(value(&inputs, "ship"), Some(InputValue::Bool(true)));
    assert_eq!(value(&inputs, "todo"), text("implement"));
    assert_eq!(value(&inputs, "style"), text("be brief"));
    let goal = inputs.iter().find(|input| input.name == "goal").unwrap();
    assert_eq!(goal.source, "task.goal");
}

#[test]
fn inputs_never_list_a_human_only_artifact() {
    let (tree, leaf) = fixture();
    let paths = shared_artifact_paths(&leaf);
    assert!(
        paths
            .iter()
            .all(|path| !path.ends_with("secret.md") && !path.ends_with("plan.md"))
    );
    let definition = definition(r#"artifacts = { type = "path", from = "task.artifacts" }"#);
    let inputs = resolve(&definition, &leaf, &tree, None, &BTreeMap::new());
    let listed = value(&inputs, "artifacts").unwrap().to_text();
    assert!(!listed.contains("secret.md"));
    assert!(!listed.contains("plan.md"));
}

#[test]
fn a_human_only_plan_with_no_shared_ancestor_is_no_value() {
    let leaf = task(LEAF_DIR, Some(leaf_file()), &["plan.md"], Some(MANIFEST));
    let mut top = task("/root/projects/p", None, &["plan.md"], None);
    top.children.push(leaf.clone());
    // The top group has no task.toml, so its new files are human-only.
    let tree = tree(top);
    let definition = definition(r#"plan = { type = "path", from = "task.plan" }"#);
    let inputs = resolve(&definition, &leaf, &tree, None, &BTreeMap::new());
    assert_eq!(inputs[0].value, None);
    assert!(inputs[0].missing);
}

#[test]
fn a_required_input_without_a_value_is_missing() {
    let mut file = leaf_file();
    file.result = None;
    file.base = None;
    let leaf = task(LEAF_DIR, Some(file), &[], None);
    let tree = tree(task("/root/projects/p", None, &[], None));
    let definition = definition(
        r#"pr = { type = "text", from = "task.pr" }
base = { type = "text", from = "task.base", required = false }
todo = { type = "text", from = "todo.name" }"#,
    );
    let inputs = resolve(&definition, &leaf, &tree, None, &BTreeMap::new());
    let missing = |name: &str| {
        inputs
            .iter()
            .find(|input| input.name == name)
            .unwrap()
            .missing
    };
    assert!(missing("pr"));
    assert!(!missing("base"), "optional inputs are never missing");
    assert!(missing("todo"), "no todo index");
    assert_eq!(value(&inputs, "base"), None);
}

#[test]
fn an_empty_goal_is_missing() {
    let leaf = task(LEAF_DIR, Some(TaskFile::new("", Kind::PrShip)), &[], None);
    let tree = tree(task("/root/projects/p", None, &[], None));
    let definition = definition(r#"goal = { type = "text", from = "task.goal" }"#);
    assert!(resolve(&definition, &leaf, &tree, None, &BTreeMap::new())[0].missing);
}

#[test]
fn overrides_win_over_sources() {
    let (tree, leaf) = fixture();
    let definition = definition(
        r#"goal = { type = "text", from = "task.goal" }
pr = { type = "text", from = "task.pr" }"#,
    );
    let overrides = BTreeMap::from([
        (
            "goal".to_string(),
            InputValue::Text("Other goal".to_string()),
        ),
        ("unknown".to_string(), InputValue::Bool(false)),
    ]);
    let inputs = resolve(&definition, &leaf, &tree, None, &overrides);
    assert_eq!(inputs.len(), 2, "overrides add no inputs");
    let goal = inputs.iter().find(|input| input.name == "goal").unwrap();
    assert_eq!(goal.value, text("Other goal"));
    assert_eq!(goal.source, OVERRIDE_SOURCE);
    assert_eq!(value(&inputs, "pr"), text("https://example.com/pr/1"));
}

#[test]
fn several_prs_are_listed() {
    let mut file = leaf_file();
    file.result = Some(TaskResult {
        kind: ResultKind::Prs,
        url: None,
        state: None,
        urls: vec!["https://e/1".to_string(), "https://e/2".to_string()],
        extra: Extra::new(),
    });
    let leaf = task(LEAF_DIR, Some(file), &[], None);
    let tree = tree(task("/root/projects/p", None, &[], None));
    let definition = definition(
        r#"prs = { type = "text", from = "task.prs" }
pr = { type = "text", from = "task.pr", required = false }"#,
    );
    let inputs = resolve(&definition, &leaf, &tree, None, &BTreeMap::new());
    assert_eq!(
        value(&inputs, "prs").unwrap().to_text(),
        "https://e/1\nhttps://e/2"
    );
    assert_eq!(value(&inputs, "pr"), None);
}
