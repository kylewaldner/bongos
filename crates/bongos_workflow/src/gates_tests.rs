//! Tests of gates, the `workflows` needs-you source, the toasts and the status chip. A child of
//! the lifecycle tests, so it shares their world, definitions and scripted runner.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use bongos_work::NeedsYou;
use bongos_work_model::rollup::rollup;
use bongos_workflow_model::record::Lifecycle as RunLifecycle;
use bongos_workflow_model::{Limits, parse};
use tempfile::TempDir;

use super::*;
use crate::gates::{self, Announced, Delivered};
use crate::runtime::{RunSpec, Runtime};
use crate::status_item::Summary;

/// A fan-out whose branch `a` waits at a gate before it starts.
const GATED_FAN: &str = r#"
schema = 1
name = "gatedfan"
start = "split"

[nodes.split]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = ["a", "b", "c"] }]

[nodes.a]
kind = "agent"
prompt = "work.md"
gate = { when = "before", question = "Go ahead?" }
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.b]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.c]
kind = "agent"
prompt = "work.md"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "agent"
prompt = "work.md"
join = { wait = "all" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn world() -> World {
    let world = World::new();
    write_definition(world.profile(), "gatedfan", GATED_FAN);
    world
}

/// Starts `gatedfan` and runs until branch `a` holds at its gate while `b` and `c` run.
fn at_the_gate(world: &World, lifecycle: &mut Lifecycle<FakeRunner>) -> String {
    let run = world.start(lifecycle, with_workflow("gatedfan")).unwrap();
    settle(lifecycle);
    // The split finishes; its three branches start, `a` stopping at its gate.
    lifecycle.runtime_mut().runner_mut().release_all();
    settle(lifecycle);
    run
}

fn leaf_needs(reason: &str) -> NeedsYou {
    NeedsYou {
        task_id: LEAF.to_string(),
        reason: reason.to_string(),
    }
}

#[test]
fn a_gate_shows_needs_you_on_the_leaf_and_every_ancestor_while_another_branch_runs() {
    let world = world();
    let mut lifecycle = world.lifecycle();
    let started = Instant::now();
    let run = at_the_gate(&world, &mut lifecycle);
    assert!(started.elapsed() < Duration::from_secs(1));

    let state = lifecycle.state(&run).unwrap();
    assert!(
        state
            .open_gates
            .contains_key(&state.open_gates.keys().next().cloned().unwrap())
    );
    assert!(!state.running.is_empty(), "the other branches run");
    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
    assert_eq!(
        gates::needs_you(&lifecycle),
        [leaf_needs("gate: Go ahead?")]
    );

    // Feature 03 rolls the leaf up to every ancestor.
    let needing: HashSet<String> = gates::needs_you(&lifecycle)
        .into_iter()
        .map(|item| item.task_id)
        .collect();
    let group = &world.tree.projects[0].task;
    assert_eq!(rollup(group, &world.tree, &needing).needs_you, 1);
    assert_eq!(
        rollup(&group.children[0], &world.tree, &needing).needs_you,
        1
    );
}

#[test]
fn in_a_fan_out_a_gate_holds_its_branch_only() {
    let world = world();
    let mut lifecycle = world.lifecycle();
    let run = at_the_gate(&world, &mut lifecycle);
    let started = started_nodes(&lifecycle);
    assert!(started.contains(&"b".to_string()) && started.contains(&"c".to_string()));
    assert!(!started.contains(&"a".to_string()), "{started:?}");
    let running: Vec<String> = lifecycle
        .state(&run)
        .unwrap()
        .running
        .keys()
        .map(|key| key.node.clone())
        .collect();
    assert_eq!(running, ["b", "c"]);
    // Only the gated branch waits; the run is not parked.
    assert_eq!(lifecycle.open_gates(&run).len(), 1);
    // The other branches finish; the gated one still holds, and the join does not start.
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.open_gates(&run).len(), 1);
    assert!(!started_nodes(&lifecycle).contains(&"gather".to_string()));
    assert_eq!(lifecycle.status(&run), Some(RunStatus::WaitingOnYou));
}

#[test]
fn the_chip_names_the_running_nodes_and_counts_the_runs_that_need_you() {
    let world = world();
    let mut lifecycle = world.lifecycle();
    at_the_gate(&world, &mut lifecycle);
    let summary = Summary::of(&lifecycle);
    assert_eq!(summary.running, ["b", "c"]);
    assert_eq!(summary.needing, 1);
    assert_eq!(
        summary.text().as_deref(),
        Some("Workflows: 1 need you; running b, c")
    );
    assert_eq!(Summary::default().text(), None);
}

#[test]
fn a_failed_run_needs_you_on_the_leaf_and_every_ancestor() {
    let world = world();
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("work", 1);
    let run = world
        .start(&mut lifecycle, with_workflow("single"))
        .unwrap();
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));
    let found = gates::needs_you(&lifecycle);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].task_id, LEAF);
    assert!(found[0].reason.starts_with("run failed: "), "{found:?}");
    let needing: HashSet<String> = [LEAF.to_string()].into();
    let group = &world.tree.projects[0].task;
    assert_eq!(rollup(group, &world.tree, &needing).needs_you, 1);
}

fn gated_spec(root: &std::path::Path) -> RunSpec {
    RunSpec {
        id: "01890a5d-ac96-774b-bcce-b30200000001".to_string(),
        definition: parse(GATED_FAN).unwrap(),
        attempt_dir: root.join("attempt"),
    }
}

#[test]
fn a_reload_keeps_the_gate_open_and_an_interrupted_run_says_so() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(gated_spec(temp.path())).unwrap();
    runtime.tick();
    runtime.runner_mut().release_all();
    runtime.tick();
    let id = gated_spec(temp.path()).id;
    assert_eq!(runtime.state(&id).unwrap().open_gates.len(), 1);
    // The app quits with the other branches running.
    runtime.quit();
    drop(runtime);

    let mut relaunched = Runtime::new(Limits::default(), FakeRunner::new());
    relaunched.load(gated_spec(temp.path())).unwrap();
    relaunched.tick();
    let state = relaunched.state(&id).unwrap();
    assert_eq!(state.open_gates.len(), 1, "the gate is in the log");
    assert_eq!(RunStatus::of(&state), RunStatus::WaitingOnYou);
    assert!(relaunched.runner().started.is_empty());
    let reasons = gates::reasons(&state);
    assert!(
        reasons.contains(&"gate: Go ahead?".to_string()),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&"run interrupted".to_string()),
        "{reasons:?}"
    );
    assert_ne!(state.lifecycle(), RunLifecycle::Queued);
}

#[test]
fn one_toast_per_gate_or_failure_not_per_update() {
    let world = world();
    let mut lifecycle = world.lifecycle();
    let run = at_the_gate(&world, &mut lifecycle);
    let state = lifecycle.state(&run).unwrap();
    let mut announced = Announced::default();
    let first = announced.notices(&run, &state);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].message, "Workflow needs you at a: Go ahead?");
    // The same gate seen again by later updates shows nothing.
    assert!(announced.notices(&run, &state).is_empty());
    finish_all(&mut lifecycle);
    assert!(
        announced
            .notices(&run, &lifecycle.state(&run).unwrap())
            .is_empty()
    );
    // Approving closes it; a failure then earns exactly one.
    let key = lifecycle.open_gates(&run)[0].clone();
    lifecycle.approve(&run, &key, None).unwrap();
    let approved = lifecycle.state(&run).unwrap();
    assert!(announced.notices(&run, &approved).is_empty());

    let mut failing = world.lifecycle();
    failing.runtime_mut().runner_mut().fail("work", 1);
    let failed = world.start(&mut failing, with_workflow("single")).unwrap();
    settle(&mut failing);
    finish_all(&mut failing);
    let state = failing.state(&failed).unwrap();
    let notices = announced.notices(&failed, &state);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].message.starts_with("Workflow run failed: "));
    assert!(announced.notices(&failed, &state).is_empty());
}

#[gpui::test]
fn the_engine_tells_the_sources_and_toasts_once(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        WorkflowsSettings::register(cx);
    });
    let world = world();
    let worktrees = DirWorktrees {
        root: world.root.join("runs"),
        error: world.worktree_error.clone(),
    };
    let lifecycle = Lifecycle::new(
        Box::new(FakeRunner::new()) as EngineRunner,
        Box::new(worktrees),
        world.settings(),
    );
    let engine = WorkflowEngine::with_lifecycle(world.dirs.clone(), lifecycle);
    cx.update(|cx| {
        crate::sources::register(&engine, cx);
        crate::gates::register(&engine, cx);
    });
    let run = cx
        .update(|cx| engine.start_in(with_workflow("gatedfan"), &world.tree, cx))
        .unwrap();
    for _ in 0..3 {
        engine.lifecycle().tick();
        cx.update(|cx| engine.publish(cx));
    }
    {
        let mut lifecycle = engine.lifecycle();
        lifecycle.runtime_mut().runner_mut().release_all();
    }
    for _ in 0..3 {
        engine.lifecycle().tick();
        cx.update(|cx| engine.publish(cx));
    }
    let collected = cx.update(|cx| bongos_work::NeedsYouRegistry::collect(cx));
    assert_eq!(
        collected.get(LEAF).map(Vec::as_slice),
        Some(["gate: Go ahead?".to_string()].as_slice())
    );
    assert_eq!(engine.open_gates(&run).len(), 1);
    let delivered = cx.update(|cx| cx.default_global::<Delivered>().0.clone());
    assert_eq!(delivered.len(), 1, "{delivered:?}");
}
