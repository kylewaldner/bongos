//! Tests of the shipped definitions (feature 20, step 20): each validates clean against an empty
//! MCP set, names no host or model in its prompts, and runs to its success end with the scripted
//! [`FakeRunner`]; the loops stop at their bounds. The scripts and prompts are not run here: no
//! agent, `gh` or network is involved.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use bongos_workflow_model::definition::{Definition, FieldType, parse};
use bongos_workflow_model::fake::{FakeRunner, Scripted};
use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::{EndState, RunState};
use bongos_workflow_model::template::references;
use bongos_workflow_model::{FailureReason, LayerDirs, Limits};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::definitions::{
    DefinitionError, SHIPPED_FILES, materialize, new_definition_from, validate_definitions,
};
use crate::shipped::{FILES, TEMPLATES, files_of, template};

fn contents(dir: &str, file: &str) -> &'static str {
    FILES
        .iter()
        .find(|shipped| shipped.path == format!("{dir}/{file}"))
        .unwrap_or_else(|| panic!("{dir}/{file} is not shipped"))
        .contents
}

fn definition(dir: &str) -> Definition {
    parse(contents(dir, DEFINITION_FILE)).unwrap()
}

fn runner(dir: &str) -> FakeRunner {
    FakeRunner::new(definition(dir), Limits::default())
}

fn ok(value: Value) -> Scripted {
    Scripted::ok(value)
}

fn assert_succeeded(state: &RunState) {
    assert_eq!(state.ended, Some((EndState::Succeeded, None)), "{state:#?}");
}

/// The end the failed run reached and the reason the definition gives for it.
fn failed_end(state: &RunState) -> String {
    match &state.ended {
        Some((EndState::Failed, Some(failure))) => {
            assert_eq!(failure.reason, FailureReason::FailureEndReached);
            failure.detail.clone()
        }
        other => panic!("expected a failed end, got {other:?}"),
    }
}

fn shipped_dir_on_disk() -> (TempDir, LayerDirs) {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("shipped");
    materialize(&dir, SHIPPED_FILES).unwrap();
    let dirs = LayerDirs {
        shipped: Some(dir),
        ..LayerDirs::default()
    };
    (temp, dirs)
}

#[test]
fn the_shipped_set_has_the_four_definitions_and_every_file_belongs_to_one() {
    let names: Vec<_> = TEMPLATES.iter().map(|template| template.name).collect();
    assert_eq!(
        names,
        ["implement PR", "implement tasks", "ship PR", "babysit PR"]
    );
    let paths: BTreeSet<_> = FILES.iter().map(|file| file.path).collect();
    assert_eq!(paths.len(), FILES.len(), "no file is listed twice");
    for file in FILES {
        assert!(
            TEMPLATES
                .iter()
                .any(|template| file.path.starts_with(&format!("{}/", template.dir))),
            "{} belongs to no template",
            file.path
        );
    }
    for shipped in TEMPLATES {
        assert_eq!(definition(shipped.dir).name, shipped.name);
        assert_eq!(
            definition(shipped.dir).description.as_deref(),
            Some(shipped.description)
        );
        assert!(files_of(shipped).any(|(relative, _)| relative == DEFINITION_FILE));
    }
    assert_eq!(template("ship-pr"), template("ship PR"));
    assert!(template("nothing").is_none());
}

#[test]
fn every_shipped_definition_validates_clean_against_an_empty_mcp_set() {
    let (_temp, dirs) = shipped_dir_on_disk();
    let report = validate_definitions(&dirs, None).unwrap();
    assert_eq!(report.definitions.len(), TEMPLATES.len());
    assert_eq!(report.error_count(), 0, "{}", report.render());
    assert_eq!(report.warning_count(), 0, "{}", report.render());
}

#[test]
fn every_shipped_file_is_written_once_and_the_scripts_are_python() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("shipped").join("1.0.0");
    assert_eq!(materialize(&dir, SHIPPED_FILES).unwrap(), FILES.len());
    assert_eq!(materialize(&dir, SHIPPED_FILES).unwrap(), 0);
    for file in FILES {
        assert_eq!(
            fs::read_to_string(dir.join(file.path)).unwrap(),
            file.contents
        );
        assert!(!file.contents.trim().is_empty(), "{} is empty", file.path);
    }
}

#[test]
fn prompts_name_no_host_and_models_are_the_newest_family() {
    let allowed_models = ["sonnet", "claude-opus-5-5"];
    for shipped in TEMPLATES {
        for (relative, text) in files_of(shipped) {
            if relative.ends_with(".md") {
                let lower = text.to_lowercase();
                for word in [
                    "github",
                    "gitlab",
                    "bitbucket",
                    "claude",
                    "opus",
                    "sonnet",
                    "haiku",
                ] {
                    assert!(
                        !lower.contains(word),
                        "{}/{relative} mentions '{word}'",
                        shipped.dir
                    );
                }
            }
        }
        let definition = definition(shipped.dir);
        let models = definition.defaults.model.iter().chain(
            definition
                .nodes
                .iter()
                .filter_map(|node| node.settings.model.as_ref()),
        );
        for model in models {
            assert!(
                allowed_models.contains(&model.as_str()),
                "{} names model '{model}'",
                shipped.dir
            );
        }
    }
}

/// Every `{{name}}` of a prompt is something the run can supply: a required input, a declared
/// field of a node that is not optional, or `item` in the node a fan-out feeds.
#[test]
fn prompt_references_name_declared_inputs_and_fields() {
    for shipped in TEMPLATES {
        let definition = definition(shipped.dir);
        let fan_targets: BTreeSet<&str> = definition
            .nodes
            .iter()
            .flat_map(|node| &node.routes)
            .filter_map(|route| route.fan_out.as_ref())
            .map(|fan_out| fan_out.to.as_str())
            .collect();
        for node in &definition.nodes {
            let Some(prompt) = &node.prompt else { continue };
            let text = contents(shipped.dir, &prompt.value);
            for reference in references(text).unwrap() {
                let name = reference.name.as_str();
                let known = if let Some(input) = name.strip_prefix("inputs.") {
                    definition
                        .inputs
                        .iter()
                        .any(|declared| declared.name == input && declared.required)
                } else if name == "item" {
                    fan_targets.contains(node.name.as_str())
                } else if let Some((from, field)) = name
                    .strip_prefix("nodes.")
                    .and_then(|rest| rest.split_once('.'))
                {
                    definition.node(from).is_some_and(|from| {
                        from.output
                            .iter()
                            .any(|declared| declared.name == field && !declared.optional)
                    })
                } else {
                    false
                };
                assert!(
                    known,
                    "{}/{}: '{{{{{name}}}}}' is not available",
                    shipped.dir, prompt.value
                );
            }
        }
    }
}

#[test]
fn programs_declare_the_permissions_they_need_and_no_mcp() {
    for shipped in TEMPLATES {
        let definition = definition(shipped.dir);
        for node in &definition.nodes {
            if let Some(permissions) = &node.permissions {
                assert!(permissions.mcp.is_empty(), "{}", node.name);
            }
            // A script that talks to the repository host declares the host.
            if let Some(script) = &node.script {
                let text = contents(shipped.dir, &script.value);
                if text.contains("\"gh\"") {
                    let permissions = node.permissions.as_ref().expect("gh needs permissions");
                    assert!(permissions.commands.contains(&"gh".to_string()));
                    assert!(!permissions.network.is_empty(), "{}", node.name);
                }
            }
        }
    }
}

fn implement_pr_run(mode: &str, items: &[&str]) -> FakeRunner {
    let mut run = runner("implement-pr");
    run.script("triage", [ok(json!({ "mode": mode, "why": "because" }))])
        .script(
            "plan",
            [ok(json!({ "items": items, "design": "DESIGN.md" }))],
        )
        .script("implement", [ok(json!({ "summary": "done" }))])
        .script("implement_item", [ok(json!({ "summary": "item done" }))])
        .script("fix", [ok(json!({ "summary": "fixed" }))])
        .script("test", [ok(json!({ "status": "pass" }))])
        .script(
            "review",
            [ok(json!({ "verdict": "approve", "pr": "Add the thing" }))],
        )
        .script("merge", [ok(json!({ "result": "merged" }))]);
    run
}

#[test]
fn implement_pr_one_shot_runs_to_its_success_end() {
    let mut run = implement_pr_run("one-shot", &[]);
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("implement"), 1);
    assert_eq!(run.starts_of("plan"), 0);
    assert_eq!(run.starts_of("implement_item"), 0);
    assert_eq!(run.starts_of("test"), 1);
    assert_eq!(run.starts_of("merge"), 1);
}

#[test]
fn implement_pr_planned_fans_out_one_agent_per_item_and_joins_at_the_tests() {
    let mut run = implement_pr_run("planned", &["parser", "printer", "docs"]);
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("plan"), 1);
    assert_eq!(run.starts_of("implement_item"), 3);
    assert_eq!(run.starts_of("implement"), 0);
    assert_eq!(run.starts_of("test"), 1, "the join waits for every item");
    assert!(run.max_branches <= 3, "limited by max_parallel_branches");
}

#[test]
fn implement_pr_loops_through_fix_when_tests_fail_and_the_review_asks_for_changes() {
    let mut run = implement_pr_run("one-shot", &[]);
    run.script(
        "test",
        [
            ok(json!({ "status": "fail" })),
            ok(json!({ "status": "pass" })),
        ],
    )
    .script(
        "review",
        [
            ok(json!({ "verdict": "changes", "pr": "Add the thing" })),
            ok(json!({ "verdict": "approve", "pr": "Add the thing" })),
        ],
    );
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("fix"), 2);
    assert_eq!(run.starts_of("test"), 3);
    assert_eq!(run.starts_of("review"), 2);
}

#[test]
fn implement_pr_stops_after_three_fix_passes() {
    let mut run = implement_pr_run("one-shot", &[]);
    run.script("test", [ok(json!({ "status": "fail" }))]);
    let message = failed_end(&run.run());
    assert_eq!(run.starts_of("fix"), 3);
    assert!(message.contains("three fix passes"), "{message}");
    assert_eq!(run.starts_of("review"), 0);
}

#[test]
fn implement_pr_fails_on_a_merge_conflict() {
    let mut run = implement_pr_run("one-shot", &[]);
    run.script("merge", [ok(json!({ "result": "conflict" }))]);
    let message = failed_end(&run.run());
    assert!(message.contains("merge conflict"), "{message}");
}

#[test]
fn implement_tasks_is_one_agent_node() {
    let definition = definition("implement-tasks");
    assert_eq!(definition.nodes.len(), 1);
    let mut run = runner("implement-tasks");
    run.script("implement", [ok(json!({ "summary": "done" }))]);
    assert_succeeded(&run.run());
    assert_eq!(run.started.len(), 1);
}

fn ship_pr_run() -> FakeRunner {
    let mut run = runner("ship-pr");
    run.script("rebase", [ok(json!({ "result": "unchanged" }))])
        .script("resolve", [ok(json!({ "summary": "resolved" }))])
        .script("push", [ok(json!({ "result": "pushed" }))])
        .script("ci", [ok(json!({ "result": "green" }))])
        .script("fix", [ok(json!({ "summary": "fixed" }))])
        .script("merge", [ok(json!({ "result": "merged" }))])
        .script("deploy", [ok(json!({ "result": "satisfied" }))]);
    run
}

#[test]
fn ship_pr_merges_a_green_pr_and_waits_for_the_deploy() {
    let mut run = ship_pr_run();
    let state = run.run();
    assert_succeeded(&state);
    let order: Vec<_> = run.started.iter().map(|key| key.node.as_str()).collect();
    assert_eq!(order, ["rebase", "ci", "merge", "deploy"]);
    assert!(
        definition("ship-pr")
            .node("merge")
            .is_some_and(|node| node.gate.is_some()),
        "a gate before merging"
    );
}

#[test]
fn ship_pr_polls_while_ci_waits_and_sleeps_the_deploy() {
    let mut run = ship_pr_run();
    run.script(
        "ci",
        [
            ok(json!({ "result": "waiting" })),
            ok(json!({ "result": "waiting" })),
            ok(json!({ "result": "green" })),
        ],
    )
    .script(
        "deploy",
        [
            ok(json!({ "result": "waiting" })),
            ok(json!({ "result": "satisfied" })),
        ],
    );
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("ci"), 3, "one pass, polled three times");
    assert_eq!(run.starts_of("fix"), 0, "no agent runs while waiting");
    assert_eq!(run.starts_of("deploy"), 2);
}

#[test]
fn ship_pr_fixes_a_red_check_pushes_and_checks_again() {
    let mut run = ship_pr_run();
    run.script(
        "ci",
        [
            ok(json!({ "result": "red" })),
            ok(json!({ "result": "green" })),
        ],
    );
    assert_succeeded(&run.run());
    let order: Vec<_> = run.started.iter().map(|key| key.node.as_str()).collect();
    assert_eq!(
        order,
        ["rebase", "ci", "fix", "push", "ci", "merge", "deploy"]
    );
}

#[test]
fn ship_pr_resolves_a_rebase_conflict_with_an_agent() {
    let mut run = ship_pr_run();
    run.script("rebase", [ok(json!({ "result": "conflict" }))]);
    assert_succeeded(&run.run());
    let order: Vec<_> = run.started.iter().map(|key| key.node.as_str()).collect();
    assert_eq!(
        order,
        ["rebase", "resolve", "push", "ci", "merge", "deploy"]
    );
}

#[test]
fn ship_pr_stops_after_five_fix_passes() {
    let mut run = ship_pr_run();
    run.script("ci", [ok(json!({ "result": "red" }))]);
    let message = failed_end(&run.run());
    assert_eq!(run.starts_of("fix"), 5);
    assert!(message.contains("five fix passes"), "{message}");
    assert_eq!(run.starts_of("merge"), 0);
}

#[test]
fn ship_pr_fails_when_the_push_is_rejected_or_the_merge_is_blocked() {
    let mut run = ship_pr_run();
    run.script("rebase", [ok(json!({ "result": "conflict" }))])
        .script("push", [ok(json!({ "result": "rejected" }))]);
    assert!(failed_end(&run.run()).contains("push was rejected"));

    let mut run = ship_pr_run();
    run.script("merge", [ok(json!({ "result": "blocked" }))]);
    assert!(failed_end(&run.run()).contains("could not be merged"));
}

fn babysit_run() -> FakeRunner {
    let mut run = runner("babysit-pr");
    run.script("check", [ok(json!({ "result": "green" }))])
        .script("fix", [ok(json!({ "summary": "fixed" }))])
        .script("push", [ok(json!({ "result": "pushed" }))])
        .script("merge", [ok(json!({ "result": "merged" }))]);
    run
}

#[test]
fn babysit_pr_waits_without_an_agent_then_merges_when_green() {
    let mut run = babysit_run();
    run.script(
        "check",
        [
            ok(json!({ "result": "waiting" })),
            ok(json!({ "result": "waiting" })),
            ok(json!({ "result": "green" })),
        ],
    );
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("check"), 3);
    assert_eq!(run.starts_of("fix"), 0);
    assert_eq!(run.starts_of("merge"), 1);
}

#[test]
fn babysit_pr_fixes_red_and_checks_again() {
    let mut run = babysit_run();
    run.script(
        "check",
        [
            ok(json!({ "result": "red" })),
            ok(json!({ "result": "red" })),
            ok(json!({ "result": "green" })),
        ],
    );
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("fix"), 2);
    assert_eq!(run.starts_of("push"), 2);
    assert_eq!(run.starts_of("check"), 3);
}

#[test]
fn babysit_pr_stops_at_five_passes() {
    let mut run = babysit_run();
    run.script("check", [ok(json!({ "result": "red" }))]);
    let message = failed_end(&run.run());
    assert_eq!(run.starts_of("fix"), 5, "the sixth red result ends the run");
    assert_eq!(run.starts_of("check"), 6);
    assert_eq!(run.starts_of("merge"), 0);
    assert!(message.contains("five fix passes"), "{message}");
}

#[test]
fn babysit_pr_ends_when_the_pr_merges_on_its_own_or_closes() {
    let mut run = babysit_run();
    run.script("check", [ok(json!({ "result": "merged" }))]);
    assert_succeeded(&run.run());
    assert_eq!(run.starts_of("merge"), 0, "someone else merged it");

    let mut run = babysit_run();
    run.script("check", [ok(json!({ "result": "closed" }))]);
    assert!(failed_end(&run.run()).contains("closed without merging"));
}

#[test]
fn a_scripted_failure_of_a_shipped_node_fails_the_run() {
    let mut run = ship_pr_run();
    run.script("ci", [Scripted::Fail(FailureReason::ProgramExitedNonzero)]);
    let state = run.run();
    assert!(
        matches!(state.ended, Some((EndState::Failed, _))),
        "{state:#?}"
    );
}

#[test]
fn new_from_a_template_copies_every_file_and_renames_the_workflow() {
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    let ship = template("ship PR").unwrap();
    let created = new_definition_from(Some(&profile), Some("ship it"), Some(ship)).unwrap();
    assert_eq!(created.name, "ship it");
    assert_eq!(created.dir, profile.join("ship-it"));
    for (relative, text) in files_of(ship) {
        let copied = fs::read_to_string(created.dir.join(relative)).unwrap();
        if relative == DEFINITION_FILE {
            assert!(copied.contains("name = \"ship it\"\n"), "{copied}");
            assert!(!copied.contains("name = \"ship PR\""));
            assert_eq!(copied.lines().count(), text.lines().count());
        } else {
            assert_eq!(copied, text, "{relative}");
        }
    }
    let dirs = LayerDirs {
        profile: Some(profile.clone()),
        ..LayerDirs::default()
    };
    let report = validate_definitions(&dirs, None).unwrap();
    assert_eq!(report.error_count(), 0, "{}", report.render());
    assert_eq!(report.warning_count(), 0, "{}", report.render());
}

#[test]
fn new_from_a_template_defaults_to_its_name_and_never_overwrites() {
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    for shipped in TEMPLATES {
        let created = new_definition_from(Some(&profile), None, Some(shipped)).unwrap();
        assert_eq!(created.name, shipped.name);
        assert!(created.file.is_file());
        assert!(matches!(
            new_definition_from(Some(&profile), None, Some(shipped)),
            Err(DefinitionError::NameTaken(..))
        ));
    }
    assert_eq!(
        Path::new(&profile).read_dir().unwrap().count(),
        TEMPLATES.len()
    );
}
