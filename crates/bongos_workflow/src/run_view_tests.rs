//! Tests of the run view: what it shows of a run (and never of the worktree), the list and graph
//! toggle, and the approve of a gate card. A child of the lifecycle tests, so it shares their
//! world, definitions and scripted runner.

use bongos_workflow_model::Limits;
use gpui::{AppContext as _, Entity};

use super::*;
use crate::run_view::{Mode, RunView, Snapshot, dump, try_dir};

const MARKER: &str = "WORKTREE-MARKER-9f3a1";

const GATED: &str = r#"
schema = 1
name = "gated"
start = "work"

[nodes.work]
kind = "agent"
prompt = "work.md"
gate = { when = "before", question = "Go ahead?" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn world() -> World {
    let world = World::new();
    write_definition(world.profile(), "gated", GATED);
    world
}

fn engine_of(world: &World, cx: &mut TestAppContext) -> WorkflowEngine {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        WorkflowsSettings::register(cx);
    });
    let worktrees = DirWorktrees {
        root: world.root.join("runs"),
        error: world.worktree_error.clone(),
    };
    let lifecycle = Lifecycle::new(
        Box::new(FakeRunner::new()) as EngineRunner,
        Box::new(worktrees),
        world.settings(),
    );
    WorkflowEngine::with_lifecycle(world.dirs.clone(), lifecycle)
}

fn start(
    engine: &WorkflowEngine,
    world: &World,
    workflow: &str,
    cx: &mut TestAppContext,
) -> String {
    let run = cx
        .update(|cx| engine.start_in(with_workflow(workflow), &world.tree, cx))
        .unwrap();
    for _ in 0..4 {
        engine.lifecycle().tick();
        cx.update(|cx| engine.publish(cx));
    }
    run
}

fn view(engine: &WorkflowEngine, run: &str, cx: &mut TestAppContext) -> Entity<RunView> {
    let engine = engine.clone();
    let run = run.to_string();
    cx.new(|cx| RunView::new(engine, run, cx))
}

#[gpui::test]
fn the_view_never_shows_what_is_in_the_worktree(cx: &mut TestAppContext) {
    let world = world();
    let engine = engine_of(&world, cx);
    let run = start(&engine, &world, "single", cx);
    let (worktree, attempt) = {
        let lifecycle = engine.lifecycle();
        (
            lifecycle.worktree(&run).unwrap().path.clone(),
            lifecycle.current_attempt_dir(&run).unwrap(),
        )
    };
    fs::write(worktree.join("secret.txt"), MARKER).unwrap();
    fs::write(worktree.join("work.md"), MARKER).unwrap();
    // A log in the run's record is shown; the worktree is not read.
    let key = NodeKey::new("work", 1);
    let dir = try_dir(&attempt, &key, 1);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("stdout.log"), "a line of the log\n").unwrap();
    engine.lifecycle().runtime_mut().runner_mut().release_all();
    for _ in 0..4 {
        engine.lifecycle().tick();
        cx.update(|cx| engine.publish(cx));
    }

    let view = view(&engine, &run, cx);
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            for _ in 0..2 {
                let text = view.text();
                assert!(text.contains("Run single"), "{text}");
                assert!(text.contains("a line of the log"), "{text}");
                assert!(!text.contains(MARKER), "{text}");
                assert!(!text.contains("secret.txt"), "{text}");
                view.toggle_graph(cx);
            }
        })
    });
}

#[gpui::test]
fn g_toggles_between_the_list_and_the_graph(cx: &mut TestAppContext) {
    let world = world();
    let engine = engine_of(&world, cx);
    let run = start(&engine, &world, "two", cx);
    let view = view(&engine, &run, cx);
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            assert_eq!(view.mode(), Mode::List);
            assert!(view.text().contains("nodes (run order)"));
            assert!(!view.text().contains("column 0"));
            view.toggle_graph(cx);
            assert_eq!(view.mode(), Mode::Graph);
            let text = view.text();
            assert!(text.contains("column 0"), "{text}");
            assert!(text.contains("plan -> work"), "{text}");
            view.toggle_graph(cx);
            assert_eq!(view.mode(), Mode::List);
        })
    });
}

#[gpui::test]
fn approving_a_gate_card_approves_the_gate(cx: &mut TestAppContext) {
    let world = world();
    let engine = engine_of(&world, cx);
    let run = start(&engine, &world, "gated", cx);
    let view = view(&engine, &run, cx);
    let key = cx.update(|cx| {
        view.update(cx, |view, _| {
            let snapshot = view.snapshot().unwrap();
            assert_eq!(snapshot.gates.len(), 1);
            assert_eq!(snapshot.gates[0].question, "Go ahead?");
            assert!(view.text().contains("gate at work: Go ahead?"));
            snapshot.gates[0].key.clone()
        })
    });
    assert_eq!(engine.open_gates(&run), vec![key.clone()]);
    cx.update(|cx| view.update(cx, |view, cx| view.approve_gate(&key, cx)))
        .unwrap();
    assert!(engine.open_gates(&run).is_empty());
    cx.update(|cx| {
        view.update(cx, |view, _| {
            assert!(view.snapshot().unwrap().gates.is_empty());
        })
    });
}

#[gpui::test]
fn the_dump_writes_the_text_of_the_view(cx: &mut TestAppContext) {
    let world = world();
    let engine = engine_of(&world, cx);
    let run = start(&engine, &world, "two", cx);
    let data =
        crate::run_view::RunData::of(&engine, &run, Limits::default().session_limit()).unwrap();
    let text = Snapshot::build(&data, None).text(Mode::List);
    let path = dump(&world.root.join("logs"), &text).unwrap();
    assert!(path.ends_with("workflow-run-view.txt"));
    assert_eq!(fs::read_to_string(path).unwrap(), text);
}
