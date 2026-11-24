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

pub(super) fn engine_of(world: &World, cx: &mut TestAppContext) -> WorkflowEngine {
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

#[gpui::test]
fn focusing_a_task_shows_its_active_run_only(cx: &mut TestAppContext) {
    let world = world();
    let engine = engine_of(&world, cx);
    assert_eq!(crate::run_view::run_to_show(&engine, Some(LEAF)), None);
    assert_eq!(crate::run_view::run_to_show(&engine, None), None);
    let run = start(&engine, &world, "gated", cx);
    assert_eq!(crate::run_view::run_to_show(&engine, Some(LEAF)), Some(run));
    assert_eq!(crate::run_view::run_to_show(&engine, Some("other")), None);
}

const CHECKED: &str = r#"
schema = 1
name = "checked"
start = "verify"

[nodes.verify]
kind = "program"
use = "checks"
routes = [
  { when = { verdict = "pass" }, to = "done" },
  { when = { verdict = "fail" }, to = "broken" },
]

[ends.done]
kind = "success"

[ends.broken]
kind = "failure"
"#;

/// Answers one line for every key it is asked about, and remembers what it was asked.
struct FixedStatus(std::rc::Rc<std::cell::RefCell<Vec<(String, NodeKey)>>>);

impl crate::node_status::NodeStatusSource for FixedStatus {
    fn status(
        &self,
        run_id: &str,
        key: &NodeKey,
        _: &gpui::App,
    ) -> Option<crate::node_status::NodeStatus> {
        self.0.borrow_mut().push((run_id.to_string(), key.clone()));
        Some(crate::node_status::NodeStatus {
            line: "tests: running part 2 of 3, 1 kept".to_string(),
            detail: Some("tests: waited 4m for test-slots".to_string()),
        })
    }
}

#[gpui::test]
fn a_running_node_shows_the_line_of_its_status_source(cx: &mut TestAppContext) {
    let world = world();
    write_definition(world.profile(), "checked", CHECKED);
    let engine = engine_of(&world, cx);
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    cx.update(|cx| {
        crate::node_status::NodeStatusSources::register(
            "checks",
            std::rc::Rc::new(FixedStatus(asked.clone())),
            cx,
        )
    });
    let run = start(&engine, &world, "checked", cx);
    let view = view(&engine, &run, cx);
    let text = cx.update(|cx| view.read(cx).text());
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim_start().starts_with("verify"))
        .expect("the node's row");
    assert_eq!(
        lines[at + 1],
        "    tests: running part 2 of 3, 1 kept",
        "{text}"
    );
    assert!(
        text.contains("  live: tests: waited 4m for test-slots"),
        "{text}"
    );
    // A checks node holds no session slot.
    assert!(text.contains("sessions 0 of"), "{text}");
    assert!(
        asked
            .borrow()
            .iter()
            .all(|(asked_run, key)| *asked_run == run && key.node == "verify")
    );

    // Once the node is no longer running, it shows no line.
    engine.lifecycle().runtime_mut().cancel(&run).unwrap();
    cx.update(|cx| view.update(cx, |view, cx| view.refresh(cx)));
    let text = cx.update(|cx| view.read(cx).text());
    assert!(!text.contains("running part 2 of 3"), "{text}");
}

#[test]
fn the_view_text_gives_the_cost_cap_reason() {
    use bongos_workflow_model::{Event, NodeKey, parse, replay};
    let definition = parse(
        r#"
schema = 1
name = "single"
start = "implement"

[nodes.implement]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#,
    )
    .unwrap();
    let state = replay(&[
        Event::Created,
        Event::CapReached {
            cap_usd: 5.0,
            cost_usd: 5.12,
            key: NodeKey::new("implement", 1),
        },
        Event::Halted,
    ]);
    let data = crate::run_view::RunData {
        run: "run".to_string(),
        workflow: "single".to_string(),
        layer: "user".to_string(),
        todo: "todo".to_string(),
        definition,
        state,
        worktree: None,
        attempt_dir: None,
        session_limit: 6,
        node_status: Default::default(),
    };
    let snapshot = Snapshot::build(&data, None);
    assert_eq!(
        snapshot.cap_reason.as_deref(),
        Some("cost cap of $5.00 reached at node implement")
    );
    assert!(
        snapshot
            .text(Mode::List)
            .contains("halted: cost cap of $5.00 reached at node implement")
    );
}

// Cancelling a built-in's start gate from the run view, `workflow: cancel gate` and the verb a
// writer runs for another process.

use crate::builtin_nodes::{BuiltinNode, BuiltinNodes, BuiltinStart};
use crate::commands::{GATE_CANCELLED, NO_START_GATE};
use crate::lifecycle::PERSON_ONLY_REFUSAL;
use crate::remote::{WorkflowPayload, WorkflowVerb, execute};
use crate::runtime::Finish;
use bongos_workflow_model::FailureReason;
use bongos_workflow_model::record::EndState;
use gpui::BorrowAppContext as _;

/// A program node whose built-in asks a question before every try.
const ASKING: &str = r#"
schema = 1
name = "asking"
start = "post"

[nodes.post]
kind = "program"
use = "wait"
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// A built-in that asks before each try and counts its starts; no try ever finishes.
#[derive(Default)]
struct AskingNode(std::sync::Mutex<usize>);

impl AskingNode {
    fn starts(&self) -> usize {
        *self.0.lock().unwrap()
    }
}

impl BuiltinNode for AskingNode {
    fn start_gate(&self, _: &BuiltinStart<'_>) -> Option<String> {
        Some("Post this to #team?".to_string())
    }

    fn start(&self, _: BuiltinStart<'_>) -> Result<(), bongos_workflow_model::Failure> {
        *self.0.lock().unwrap() += 1;
        Ok(())
    }

    fn poll(&self) -> Vec<Finish> {
        Vec::new()
    }

    fn stop(&self, _: &[(String, NodeKey)]) {}
}

/// The engine of `world` over the settings already installed, with `node` as the `wait` built-in.
fn asking_engine(world: &World, node: &Arc<AskingNode>) -> WorkflowEngine {
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
    let builtins = BuiltinNodes::default();
    builtins.insert("wait", node.clone());
    engine.lifecycle().runtime_mut().set_builtins(builtins);
    engine
}

fn asking_world() -> World {
    let world = world();
    write_definition(world.profile(), "asking", ASKING);
    world
}

/// Ticks `run` a few times: a cancelled start gate must not come back as a new try.
fn tick(engine: &WorkflowEngine, cx: &mut TestAppContext) {
    for _ in 0..4 {
        engine.lifecycle().tick();
        cx.update(|cx| engine.publish(cx));
    }
}

/// The run failed on `post` with `GateCancelled`, never started it and asks nothing more.
fn assert_cancelled(engine: &WorkflowEngine, run: &str, node: &AskingNode) {
    let state = engine.lifecycle().state(run).unwrap();
    let (end, failure) = state.ended.clone().expect("the run ended");
    assert_eq!(end, EndState::Failed);
    assert_eq!(failure.unwrap().reason, FailureReason::GateCancelled);
    assert!(state.open_gates.is_empty(), "not retried: no new gate");
    assert_eq!(node.starts(), 0, "the step never started");
}

#[gpui::test]
fn cancelling_a_start_gate_card_fails_the_step_without_a_retry(cx: &mut TestAppContext) {
    let world = asking_world();
    let node = Arc::new(AskingNode::default());
    engine_of(&world, cx);
    let engine = asking_engine(&world, &node);
    let run = start(&engine, &world, "asking", cx);
    let view = view(&engine, &run, cx);
    let key = cx.update(|cx| {
        let snapshot = view.read(cx).snapshot().unwrap().clone();
        assert_eq!(snapshot.gates.len(), 1);
        assert!(snapshot.gates[0].start_gate, "a start gate card");
        snapshot.gates[0].key.clone()
    });
    assert_eq!(engine.start_gates(&run), vec![key.clone()]);

    cx.update(|cx| view.update(cx, |view, cx| view.cancel_gate(&key, cx)))
        .unwrap();
    tick(&engine, cx);
    assert_cancelled(&engine, &run, &node);
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.refresh(cx);
            assert!(view.snapshot().unwrap().gates.is_empty());
        })
    });
    // A second cancel finds no gate.
    let again = cx.update(|cx| view.update(cx, |view, cx| view.cancel_gate(&key, cx)));
    assert!(again.is_err(), "{again:?}");
}

#[gpui::test]
fn a_static_gate_card_is_not_a_start_gate_and_cannot_be_cancelled(cx: &mut TestAppContext) {
    let world = world();
    let engine = engine_of(&world, cx);
    let run = start(&engine, &world, "gated", cx);
    let view = view(&engine, &run, cx);
    let key = cx.update(|cx| {
        let snapshot = view.read(cx).snapshot().unwrap().clone();
        assert!(!snapshot.gates[0].start_gate);
        snapshot.gates[0].key.clone()
    });
    assert!(engine.start_gates(&run).is_empty());
    let cancelled = cx.update(|cx| view.update(cx, |view, cx| view.cancel_gate(&key, cx)));
    assert!(cancelled.is_err(), "{cancelled:?}");
    assert_eq!(engine.open_gates(&run), vec![key]);
}

#[gpui::test]
fn the_cancel_gate_verb_is_refused_for_a_peer_without_a_person_and_runs_for_a_window(
    cx: &mut TestAppContext,
) {
    let world = asking_world();
    let node = Arc::new(AskingNode::default());
    engine_of(&world, cx);
    let engine = asking_engine(&world, &node);
    cx.update(|cx| cx.set_global(engine.clone()));
    let payload = |run: &str| WorkflowPayload {
        run: Some(run.to_string()),
        ..WorkflowPayload::default()
    };

    let run = start(&engine, &world, "asking", cx);
    // The command line cannot attest a person: refused, and the gate stays open.
    let refused = cx.update(|cx| execute(WorkflowVerb::CancelGate, payload(&run), false, cx));
    assert_eq!(refused, Err(PERSON_ONLY_REFUSAL.to_string()));
    tick(&engine, cx);
    assert_eq!(engine.start_gates(&run).len(), 1, "the gate is still open");
    assert_eq!(engine.lifecycle().state(&run).unwrap().ended, None);

    // A window speaks for a person.
    let done = cx.update(|cx| execute(WorkflowVerb::CancelGate, payload(&run), true, cx));
    assert_eq!(done, Ok(GATE_CANCELLED.to_string()));
    tick(&engine, cx);
    assert_cancelled(&engine, &run, &node);
    // Nothing is left to cancel.
    let none = cx.update(|cx| execute(WorkflowVerb::CancelGate, payload(&run), true, cx));
    assert_eq!(none, Err(NO_START_GATE.to_string()));
    assert_eq!(
        serde_json::to_value(WorkflowVerb::CancelGate).unwrap(),
        serde_json::json!("cancel-gate")
    );
}

#[gpui::test]
async fn workflow_cancel_gate_in_a_run_view_cancels_its_start_gate(cx: &mut TestAppContext) {
    let (workspace, _, cx) = crate::actions_tests::workspace_with_actions(cx).await;
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workflows.get_or_insert_default().enabled = Some(true);
            });
        })
    });
    let world = asking_world();
    let node = Arc::new(AskingNode::default());
    let engine = asking_engine(&world, &node);
    cx.update(|_, cx| cx.set_global(engine.clone()));
    let run = start(&engine, &world, "asking", cx);
    workspace.update_in(cx, |workspace, window, cx| {
        crate::run_view::show(engine.clone(), run.clone(), workspace, window, cx)
    });
    cx.run_until_parked();

    let action = cx.update(|_, cx| cx.build_action("workflow::CancelGate", None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(cx.global::<crate::actions::LastToast>().0, GATE_CANCELLED));
    tick(&engine, cx);
    assert_cancelled(&engine, &run, &node);

    // Nothing is left to cancel.
    let action = cx.update(|_, cx| cx.build_action("workflow::CancelGate", None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(cx.global::<crate::actions::LastToast>().0, NO_START_GATE));
}
