//! Tests of the `lanes` definition folder and the prompt check of workflow validation.

use std::fs;
use std::path::Path;

use bongos_workflow::definitions::validate_definitions;
use bongos_workflow_model::{LayerDirs, Severity};
use tempfile::TempDir;

use crate::definitions::{prompt_lane_diagnostics, validate_folder};

const TABLE: &str = r#"schema = 1

[[kind]]
name = "build"
count = 2
rules = ["implement"]

[rules.implement]
workflow = "implement step"

[[pin]]
task = "0193"
kind = "build"
"#;

fn reasons(path: &str, text: &str) -> Vec<String> {
    validate_folder(Path::new(path), text)
        .into_iter()
        .map(|problem| {
            assert_eq!(problem.path, path);
            problem.reason
        })
        .collect()
}

#[test]
fn lanes_a_valid_table_lands_with_its_workflows_and_tasks_taken_as_known() {
    assert_eq!(reasons("lanes/lanes.toml", TABLE), Vec::<String>::new());
}

#[test]
fn lanes_an_invalid_table_is_refused_by_line() {
    // A kind lists a rule nobody declared, on line 6.
    let undeclared = TABLE.replace(
        "rules = [\"implement\"]",
        "rules = [\"implement\", \"ship\"]",
    );
    let found = reasons("lanes/lanes.toml", &undeclared);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with("line 6: ") && found[0].contains("\"ship\""),
        "{found:?}"
    );

    // An unknown key is named on its own line.
    let unknown = TABLE.replace("count = 2\n", "count = 2\nspeed = 3\n");
    let found = reasons("lanes/lanes.toml", &unknown);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with("line 6: ") && found[0].contains("speed"),
        "{found:?}"
    );

    // A table that does not parse names the line of the syntax error.
    let broken = TABLE.replace("count = 2", "count = = 2");
    let found = reasons("lanes/lanes.toml", &broken);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("line 5: "), "{found:?}");
}

#[test]
fn lanes_other_files_of_the_folder_are_not_judged() {
    assert!(reasons("lanes/scripts/stale", "count = = 2").is_empty());
    assert!(reasons("lanes/nested/lanes.toml", "count = = 2").is_empty());
}

const WORKFLOW: &str = r#"schema = 1
name = "implement step"
start = "implement"

[nodes.implement]
kind = "agent"
prompt = "implement.md"
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// A project's definitions in `root`: the lane table, and the workflow `implement` whose prompt
/// is `prompt`.
fn definitions(root: &TempDir, prompt: &str) -> std::path::PathBuf {
    let definitions = root.path().join("definitions");
    let lanes = definitions.join("lanes");
    fs::create_dir_all(&lanes).unwrap();
    fs::write(lanes.join("lanes.toml"), TABLE).unwrap();
    let workflow = definitions.join("workflows").join("implement");
    fs::create_dir_all(&workflow).unwrap();
    fs::write(workflow.join("workflow.toml"), WORKFLOW).unwrap();
    fs::write(workflow.join("implement.md"), prompt).unwrap();
    definitions
}

#[test]
fn lanes_a_prompt_naming_a_lane_instance_is_refused_by_line() {
    let root = TempDir::new().unwrap();
    let definitions = definitions(
        &root,
        "Implement the step.\nWhen done, hand over to build-2.\nName the kind: build.\n",
    );
    let dir = definitions.join("workflows").join("implement");
    let definition = bongos_workflow_model::parse(WORKFLOW).unwrap();
    let found = prompt_lane_diagnostics(&definition, &dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::Error);
    assert_eq!(found[0].node.as_deref(), Some("implement"));
    assert_eq!(found[0].line, 7, "the node's prompt line");
    assert!(
        found[0]
            .message
            .contains("prompt implement.md line 2 names lane build-2"),
        "{found:?}"
    );

    // Through the workflow validation entry point, once the hook is registered.
    bongos_workflow::ValidationHooks::register(prompt_lane_diagnostics);
    let layers = LayerDirs {
        project: Some(definitions.join("workflows")),
        ..LayerDirs::default()
    };
    let report = validate_definitions(&layers, None).unwrap();
    let errors: Vec<_> = report.definitions[0]
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("names lane"))
        .collect();
    assert_eq!(errors.len(), 1, "{:?}", report.definitions[0].diagnostics);
    assert_eq!(errors[0].severity, Severity::Error);
}

#[test]
fn lanes_a_landing_whose_prompt_names_a_lane_instance_is_refused_by_line() {
    let root = TempDir::new().unwrap();
    let definitions = definitions(&root, "Read the plan.\nAsk build-2 for help.\n");
    bongos_workflow::ValidationHooks::register(prompt_lane_diagnostics);
    // Check 5 (`workflow-valid`) over the change's workflows folder, as a landing runs it.
    let refusals = bongos_workflow::landing::problems(
        &definitions.join("workflows"),
        &["implement".to_string()],
    );
    assert_eq!(
        refusals,
        [
            "workflow implement step: node implement, line 7: prompt implement.md line 2 names lane \
          build-2; name the kind"
        ],
    );
}

#[test]
fn lanes_a_landing_with_an_invalid_lane_table_is_refused_by_file_and_line() {
    // Check 1 shows each problem of a registered folder as `<file>: <reason>`.
    let undeclared = TABLE.replace("rules = [\"implement\"]", "rules = [\"ship\"]");
    let problems = validate_folder(Path::new("lanes/lanes.toml"), &undeclared);
    assert_eq!(problems.len(), 1, "{problems:?}");
    let shown = problems[0].to_string();
    assert!(shown.starts_with("lanes/lanes.toml: line 6: "), "{shown}");
}

#[test]
fn lanes_a_workflow_no_dispatch_rule_names_is_not_checked() {
    let root = TempDir::new().unwrap();
    let definitions = definitions(&root, "Hand over to build-2.\n");
    let dir = definitions.join("workflows").join("implement");
    let other = WORKFLOW.replace("name = \"implement step\"", "name = \"tidy\"");
    let definition = bongos_workflow_model::parse(&other).unwrap();
    assert!(prompt_lane_diagnostics(&definition, &dir).is_empty());
}

#[test]
fn lanes_a_script_naming_a_lane_instance_is_refused_by_line() {
    let root = TempDir::new().unwrap();
    let definitions = definitions(&root, "Implement it.\n");
    let dir = definitions.join("workflows").join("implement");
    fs::write(dir.join("gather.py"), "import os\n\nprint('build-1')\n").unwrap();
    let workflow = WORKFLOW.replace(
        "[ends.done]",
        "[nodes.gather]\nkind = \"program\"\nscript = \"gather.py\"\nroutes = [{ to = \"done\" }]\n\n[ends.done]",
    );
    let definition = bongos_workflow_model::parse(&workflow).unwrap();
    let found = prompt_lane_diagnostics(&definition, &dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].node.as_deref(), Some("gather"));
    assert!(
        found[0]
            .message
            .contains("prompt gather.py line 3 names lane build-1"),
        "{found:?}"
    );
}

#[test]
fn lanes_a_prompt_naming_only_kinds_or_no_table_passes() {
    let root = TempDir::new().unwrap();
    let definitions = definitions(&root, "Hand over to a build lane, never rebuild-2.\n");
    let dir = definitions.join("workflows").join("implement");
    let definition = bongos_workflow_model::parse(WORKFLOW).unwrap();
    assert!(prompt_lane_diagnostics(&definition, &dir).is_empty());

    // Without a lane table nothing is checked.
    fs::write(dir.join("implement.md"), "Hand over to build-2.\n").unwrap();
    fs::remove_file(definitions.join("lanes").join("lanes.toml")).unwrap();
    assert!(prompt_lane_diagnostics(&definition, &dir).is_empty());
}
