//! Tests of the picker and pre-run summary of `workflow: run`: the todo's workflow preselected,
//! another pick leaving the task as it is, a missing input blocking Enter until it is supplied,
//! and the summary dump listing permissions, sandbox state, caps and inputs. A child of the
//! lifecycle tests, so it shares their world and definitions.

use bongos_workflow_model::layers::DEFINITION_FILE;

use super::*;
use crate::picker::{Choice, Picker, Review, Row, candidates, todo_workflow};
use crate::runner_claude::AgentDefaults;
use crate::runner_program::ProgramSettings;
use crate::summary::SandboxState;

/// Needs `task.pr`; an agent with its own model and mode, and a program with permissions.
const REVIEWED_PR: &str = r#"
schema = 1
name = "needspr"
description = "Review a pull request"
start = "work"

[inputs]
pr = { type = "text", from = "task.pr", required = true }
note = { type = "text", from = "literal:be kind" }

[defaults]
model = "opus"

[nodes.work]
kind = "agent"
prompt = "work.md"
permission_mode = "plan"
max_turns = 12
max_cost_usd = 2.5
max_minutes = 15
output = { summary = { type = "text" } }
routes = [{ to = "check" }]

[nodes.check]
kind = "program"
script = "check.py"
permissions = { commands = ["cargo"], network = ["github.com"], mcp = ["github"] }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn review_summary(world: &World, review: &Review, sandbox: &SandboxState) -> Summary {
    review
        .summary(
            &world.dirs,
            &world.tree,
            sandbox,
            &AgentDefaults::default(),
            &ProgramSettings::default(),
        )
        .unwrap()
}

use crate::summary::Summary;

#[test]
fn the_todos_workflow_is_preselected() {
    let world = World::new();
    let picker = Picker::new(
        candidates(&world.dirs),
        todo_workflow(&world.tree, LEAF).as_deref(),
    );
    assert_eq!(picker.chosen(), Some("single"));
    assert_eq!(picker.labels()[picker.selected()], "single (profile)");
    // A todo that names nothing, or a definition that is gone, selects the first.
    assert_eq!(Picker::new(candidates(&world.dirs), None).selected(), 0);
    assert_eq!(
        Picker::new(candidates(&world.dirs), Some("gone")).selected(),
        0
    );
}

#[test]
fn layers_are_shown_with_what_they_shadow() {
    let mut world = World::new();
    let project = world.root.join("project-layer");
    write_definition(&project, "single", SINGLE);
    world.dirs.project = Some(project);
    let labels: Vec<String> = candidates(&world.dirs)
        .into_iter()
        .map(|found| found.label)
        .collect();
    assert!(
        labels.contains(&"single (project, shadows profile)".to_string()),
        "{labels:?}"
    );
}

#[gpui::test]
fn a_different_pick_leaves_the_task_as_it_is(cx: &mut TestAppContext) {
    let world = World::new();
    let task_toml = world.root.join("task.toml");
    fs::write(&task_toml, "goal = \"Add login\"\n").unwrap();
    let before = fs::read(&task_toml).unwrap();
    let linked = linked(&world, FakeRunner::new(), cx);
    let todos_before = linked.writes.leaf().file.unwrap().todos;

    let mut picker = Picker::new(candidates(&world.dirs), Some("single"));
    let other = picker
        .candidates
        .iter()
        .position(|found| found.name == "two")
        .unwrap();
    picker.select(other);
    let review = Review::new(LEAF, picker.chosen().unwrap());
    let summary = review_summary(&world, &review, &SandboxState::Sandboxed);
    assert_eq!(summary.workflow, "two");
    assert!(matches!(
        review.choose(&Row::Start, &summary),
        Choice::Start(_)
    ));
    assert_eq!(fs::read(&task_toml).unwrap(), before);

    // Starting it runs `two` and still leaves the todo's own `workflow` as it was.
    let run = linked.start(review.request(), cx);
    assert_eq!(linked.engine.lifecycle().record(&run).unwrap().workflow, "two");
    let todos_after = linked.writes.leaf().file.unwrap().todos;
    let workflows = |todos: &[bongos_work_model::Todo]| -> Vec<Option<String>> {
        todos.iter().map(|todo| todo.workflow.clone()).collect()
    };
    assert_eq!(workflows(&todos_after), workflows(&todos_before));
    assert_eq!(fs::read(&task_toml).unwrap(), before);
}

#[gpui::test]
fn a_missing_input_blocks_enter_until_it_is_supplied(cx: &mut TestAppContext) {
    let world = World::new();
    write_definition(world.profile(), "needspr", REVIEWED_PR);
    fs::write(world.profile().join("needspr").join("check.py"), "pass\n").unwrap();
    let linked = linked(&world, FakeRunner::new(), cx);
    let mut review = Review::new(LEAF, "needspr");
    let sandbox = SandboxState::Sandboxed;

    let summary = review_summary(&world, &review, &sandbox);
    assert!(!summary.can_start());
    assert_eq!(review.choose(&Row::Start, &summary), Choice::Blocked(vec!["pr".to_string()]));
    let rows = review.rows(&summary);
    assert!(rows[0].1.contains("blocked, missing: pr"), "{rows:?}");
    assert_eq!(review.first_row(&summary), 1, "the missing input is highlighted first");
    assert_eq!(
        review.choose(&Row::Override("pr".to_string()), &summary),
        Choice::Ask {
            name: "pr".to_string(),
            initial: String::new()
        }
    );
    assert!(summary.text().contains("Missing inputs: pr"));

    review.set_override("pr", " https://example.com/pr/9 ");
    let summary = review_summary(&world, &review, &sandbox);
    assert!(summary.can_start());
    assert!(summary.text().contains("pr: https://example.com/pr/9 [from override]"));
    let Choice::Start(request) = review.choose(&Row::Start, &summary) else {
        panic!("Enter starts once the input has a value");
    };
    assert_eq!(request.workflow.as_deref(), Some("needspr"));
    let run = linked.start(request, cx);
    let lifecycle = linked.engine.lifecycle();
    assert_eq!(lifecycle.record(&run).unwrap().inputs[0].source, "override");
    drop(lifecycle);

    // An empty answer drops the override: blocked again.
    review.set_override("pr", "");
    assert!(!review_summary(&world, &review, &sandbox).can_start());
}

#[test]
fn the_summary_dump_lists_permissions_sandbox_caps_and_inputs() {
    let world = World::new();
    write_definition(world.profile(), "needspr", REVIEWED_PR);
    let mut review = Review::new(LEAF, "needspr");
    review.set_override("pr", "https://example.com/pr/9");
    let sandbox = SandboxState::NotSandboxed("no bwrap".to_string());
    let text = review_summary(&world, &review, &sandbox).text();
    let has = |line: &str| assert!(text.contains(line), "missing {line:?} in:\n{text}");
    has("Workflow needspr (profile)");
    has("Review a pull request");
    has(" * work -> check");
    has("check -> done");
    has("work [agent]");
    has("agent: claude");
    has("model: opus");
    has("permission mode: plan");
    has("caps: 12 turns, $2.50, 15 min");
    has("check [program]");
    has("runs: script check.py");
    has("commands: cargo");
    has("network: github.com");
    has("mcp: github");
    has("sandbox: not sandboxed: no bwrap");
    has("caps: 10 min");
    has("pr: https://example.com/pr/9 [from override]");
    has("note: be kind [from literal:be kind]");
    let sandboxed = review_summary(&world, &review, &SandboxState::Sandboxed).text();
    assert!(sandboxed.contains("sandbox: sandboxed"));
    // The definition file is only read.
    assert!(world.profile().join("needspr").join(DEFINITION_FILE).exists());
}
