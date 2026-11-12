use serde_json::{Value, json};

use crate::definition::{Definition, parse};
use crate::fake::{FakeRunner, Scripted};
use crate::layout::{Status, layout};
use crate::record::RunState;
use crate::scheduler::Limits;

fn runner(text: &str) -> FakeRunner {
    FakeRunner::new(parse(text).unwrap(), Limits::default())
}

fn ok(value: Value) -> Scripted {
    Scripted::Result(value)
}

fn status_of(layout: &crate::layout::GraphLayout, name: &str) -> Vec<Status> {
    layout
        .nodes
        .iter()
        .filter(|node| node.name == name)
        .map(|node| node.status)
        .collect()
}

const FAN: &str = r#"
schema = 1
name = "per item"
start = "plan"

[nodes.plan]
kind = "agent"
output = { items = { type = "list", of = "text" } }
routes = [{ fan_out = { over = "items", to = "work" } }]

[nodes.work]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "program"
join = { wait = "all" }
output = { status = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn a_fan_out_has_one_lane_per_item_under_its_node() {
    let mut run = runner(FAN);
    run.script("plan", [ok(json!({ "items": ["a", "b", "c"] }))])
        .script("work", [ok(json!({ "summary": "did it" }))])
        .script("gather", [ok(json!({ "status": "joined" }))]);
    let state = run.run();
    let graph = layout(&run.definition, &state);
    assert_eq!(
        graph.text(),
        "column 0
  lane 0 plan (agent) succeeded
column 1
  lane 0 work [plan-0] (agent) succeeded
  lane 1 work [plan-1] (agent) succeeded
  lane 2 work [plan-2] (agent) succeeded
column 2
  lane 0 gather (program) succeeded
column 3
  lane 0 done (end (success)) reached
edges
  plan -> work (each items)
  work -> gather
  gather -> done
"
    );
}

const LOOP: &str = r#"
schema = 1
name = "loop"
start = "test"

[nodes.test]
kind = "program"
output = { status = { type = "choice", values = ["pass", "fail"] } }
routes = [
  { when = { status = "pass" }, to = "done" },
  { when = { status = "fail" }, to = "fix", max_passes = 3 },
]

[nodes.fix]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "test" }]

[ends.done]
kind = "success"
"#;

#[test]
fn a_loop_is_an_arc_labeled_with_its_pass() {
    let mut run = runner(LOOP);
    run.script(
        "test",
        [
            ok(json!({ "status": "fail" })),
            ok(json!({ "status": "fail" })),
            ok(json!({ "status": "pass" })),
        ],
    )
    .script("fix", [ok(json!({ "summary": "tried" }))]);
    let state = run.run();
    let graph = layout(&run.definition, &state);
    let text = graph.text();
    assert!(
        text.contains("arcs\n  fix -> test \"pass 2 of 3\""),
        "{text}"
    );
    assert!(
        text.contains("lane 0 test (program) succeeded pass 3"),
        "{text}"
    );
    // The back edge does not push `test` right of `fix`.
    assert_eq!(graph.nodes[0].name, "test");
    assert_eq!(graph.nodes[0].column, 0);
    assert_eq!(
        graph
            .nodes
            .iter()
            .find(|node| node.name == "fix")
            .unwrap()
            .column,
        1
    );
}

const ROUTED: &str = r#"
schema = 1
name = "routed"
start = "triage"

[nodes.triage]
kind = "agent"
output = { mode = { type = "choice", values = ["one-shot", "planned"] } }
routes = [
  { when = { mode = "one-shot" }, to = "implement" },
  { when = { mode = "planned" }, to = "plan" },
]

[nodes.implement]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[nodes.plan]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

#[test]
fn a_path_not_taken_is_skipped_once_the_run_ends() {
    let mut run = runner(ROUTED);
    run.script("triage", [ok(json!({ "mode": "one-shot" }))])
        .script("implement", [ok(json!({ "summary": "done" }))]);
    let state = run.run();
    let graph = layout(&run.definition, &state);
    assert_eq!(status_of(&graph, "plan"), vec![Status::Skipped]);
    assert_eq!(status_of(&graph, "implement"), vec![Status::Succeeded]);
    assert!(graph.text().contains("plan (agent) skipped"));
}

#[test]
fn unreached_nodes_are_pending_while_the_run_is_live() {
    let definition: Definition = parse(ROUTED).unwrap();
    let graph = layout(&definition, &RunState::default());
    assert_eq!(status_of(&graph, "triage"), vec![Status::Pending]);
    assert!(Status::Pending.dimmed());
    assert!(!Status::Succeeded.dimmed());
    assert_eq!(graph.columns, 3);
}
