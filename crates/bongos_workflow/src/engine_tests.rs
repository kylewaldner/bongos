//! Tests of the engine's wiring: its tick loop (one per engine, idle with no runs), the runs on
//! disk loaded once at the first work tree, the quit hook, turning workflows off, and the runner
//! snapshot that keeps its last values when a refresh fails. A child of the lifecycle tests, so
//! it shares their world, scripted runner and in-memory task writes. Time moves only through the
//! test executor's clock.

use std::sync::atomic::{AtomicBool, Ordering};

use bongos_chat::model::Agent;
use bongos_chat::{AgentCommand, Health, HealthState};
use bongos_work::WriteError;
use bongos_work_model::commands::{Change, Context};
use bongos_workflow_model::{Failure, FailureReason};
use gpui::{BorrowAppContext as _, TestAppContext};

use super::task_link_tests::{Linked, MemoryWrites, auto_runner, linked};
use super::*;
use crate::dispatch::RunnerSnapshot;
use crate::engine::{AppReading, HealthReading, SnapshotBuilder, TICK_INTERVAL};
use crate::process;
use crate::task_link::{MERGE_GATE, TaskWrites, WriteCommand, gate_key};

/// `linked` made the app's engine: global, with its hooks and its loop, which has begun waiting.
fn attached(world: &World, runner: FakeRunner, cx: &mut TestAppContext) -> Linked {
    let linked = linked(world, runner, cx);
    cx.update(|cx| linked.engine.attach(cx));
    cx.run_until_parked();
    linked
}

/// Moves the test clock on by `passes` tick intervals, one pass of the loop each.
fn advance(cx: &mut TestAppContext, passes: usize) {
    for _ in 0..passes {
        cx.executor().advance_clock(TICK_INTERVAL);
        cx.run_until_parked();
    }
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workflows.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

/// A runner whose tries are real `sleep 60` processes, each in its own group.
fn sleeping_runner() -> FakeRunner {
    let mut runner = FakeRunner::new();
    runner.command = Some(vec!["sleep".to_string(), "60".to_string()]);
    runner
}

/// The process group of the one try in flight.
fn running_group(linked: &Linked) -> u32 {
    linked
        .runner
        .lock()
        .unwrap()
        .processes()
        .next()
        .expect("a try runs")
        .record()
        .pid
}

fn logged_events(world: &World, run: &str) -> Vec<Event> {
    let bytes = fs::read(events_of(world, run)).unwrap();
    bongos_workflow_model::record::parse_log(&bytes)
        .unwrap()
        .events
}

#[gpui::test]
fn the_loop_carries_a_one_node_run_to_its_end_and_checks_its_todo(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = attached(&world, auto_runner(), cx);
    let run = linked.start(StartRequest::new(LEAF), cx);
    // The start ticked once: the node began without waiting for the loop.
    assert_eq!(linked.runner.lock().unwrap().started.len(), 1);
    assert_eq!(linked.status(&run), RunStatus::Running);

    // Only the loop ticks now: it sees the node finish and offers the final merge.
    advance(cx, 2);
    assert_eq!(linked.status(&run), RunStatus::WaitingOnYou);
    assert_eq!(
        linked.open_gates(&run),
        [(
            MERGE_GATE.to_string(),
            crate::task_link::MERGE_QUESTION.to_string()
        )]
    );

    cx.update(|cx| linked.engine.approve(&run, &gate_key(MERGE_GATE), None, cx))
        .unwrap();
    advance(cx, 2);
    assert_eq!(linked.status(&run), RunStatus::Succeeded);
    assert_eq!(
        linked.writes.todo_states()[1],
        ("implement".to_string(), TodoState::Done)
    );
}

#[gpui::test]
fn quit_stops_the_running_process_before_the_app_goes(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = attached(&world, sleeping_runner(), cx);
    let run = linked.start(with_workflow("single"), cx);
    let group = running_group(&linked);
    assert!(process::group_alive(group));

    cx.quit();

    assert!(!process::group_alive(group), "the try outlived the quit");
    let events = logged_events(&world, &run);
    assert!(
        matches!(
            events.last(),
            Some(Event::Interrupted { keys }) if keys == &[NodeKey::new("work", 1)]
        ),
        "{events:?}"
    );
}

#[gpui::test]
fn turning_workflows_off_interrupts_live_runs_and_ends_the_loop(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = attached(&world, sleeping_runner(), cx);
    let run = linked.start(with_workflow("single"), cx);
    let group = running_group(&linked);
    assert!(process::group_alive(group));

    set_enabled(cx, false);
    cx.run_until_parked();

    assert!(!process::group_alive(group), "the try outlived turning off");
    let state = linked.engine.lifecycle().state(&run).unwrap();
    assert_eq!(
        state.interrupted.keys().collect::<Vec<_>>(),
        [&NodeKey::new("work", 1)]
    );
    assert!(cx.update(|cx| WorkflowEngine::ensure(cx)).is_none());
    // No loop runs while workflows are off.
    let passes = linked.engine.ticker().passes.load(Ordering::SeqCst);
    advance(cx, 4);
    assert_eq!(
        linked.engine.ticker().passes.load(Ordering::SeqCst),
        passes,
        "the loop ended"
    );

    // Unlike a quit, workflows on again run the loop again and the run can resume.
    set_enabled(cx, true);
    advance(cx, 2);
    assert_eq!(
        linked.engine.ticker().passes.load(Ordering::SeqCst),
        passes + 2
    );
    cx.update(|cx| linked.engine.resume(&run, cx)).unwrap();
    assert_eq!(linked.runner.lock().unwrap().started.len(), 2);
    cx.update(|cx| linked.engine.cancel(&run, cx)).unwrap();
}

#[gpui::test]
fn with_no_runs_the_one_loop_of_an_engine_does_no_io(cx: &mut TestAppContext) {
    let world = World::new();
    let linked = attached(&world, FakeRunner::new(), cx);
    // Asking for the loop again starts no second one.
    cx.update(|cx| {
        linked.engine.run_loop(cx);
        linked.engine.run_loop(cx);
    });
    cx.run_until_parked();

    advance(cx, 4);

    let ticker = linked.engine.ticker();
    assert_eq!(
        ticker.passes.load(Ordering::SeqCst),
        4,
        "one pass per interval"
    );
    assert_eq!(ticker.ticks.load(Ordering::SeqCst), 0, "nothing ticked");
    assert_eq!(
        linked.engine.runner_inputs().snapshot(),
        RunnerSnapshot::default(),
        "nothing was read for the runner"
    );
    assert!(linked.runner.lock().unwrap().started.is_empty());
    assert!(!world.root.join("work").exists(), "the load wrote nothing");
    assert!(!world.root.join("runs").exists());
}

#[test]
fn a_failed_health_or_mcp_refresh_keeps_the_last_snapshot() {
    // A `PATH` with nothing on it, so the checks find no program and run none.
    let env: BTreeMap<String, String> = [("PATH".to_string(), "/nonexistent".to_string())].into();
    let previous = RunnerSnapshot {
        claude: Some(Err(Failure::new(
            FailureReason::AgentNotInstalledOrLoggedOut,
            None,
            "the last check",
        ))),
        mcp_installed: [(AgentKind::Claude, BTreeSet::from(["github".to_string()]))].into(),
        base_env: env.clone(),
        ..RunnerSnapshot::default()
    };
    let reading = |state: Option<HealthState>, mcp: &[&str]| AppReading {
        settings: WorkflowsSettings::default(),
        health: state.map(|state| HealthReading {
            command: AgentCommand {
                command: "claude".to_string(),
                extra_args: Vec::new(),
            },
            health: Health {
                agent: Agent::Claude,
                state,
                fix: String::new(),
                chat_caps: None,
                caps: None,
            },
            env: [
                ("PATH".to_string(), "/nonexistent".to_string()),
                ("HOME".to_string(), "/home/test".to_string()),
            ]
            .into(),
        }),
        mcp: if mcp.is_empty() {
            BTreeMap::new()
        } else {
            [(
                AgentKind::Claude,
                mcp.iter().map(|server| server.to_string()).collect(),
            )]
            .into()
        },
    };
    let mut builder = SnapshotBuilder::default();

    // No health to read and no fresh MCP discovery: nothing is cleared.
    let next = builder.refresh(&previous, reading(None, &[]));
    assert_eq!(next.claude, previous.claude);
    assert_eq!(next.base_env, env);
    assert_eq!(next.mcp_installed, previous.mcp_installed);

    // A health check that did not answer keeps the last check and environment too.
    let next = builder.refresh(&next, reading(Some(HealthState::Unchecked), &[]));
    assert_eq!(next.claude, previous.claude);
    assert_eq!(next.base_env, env);

    // A real answer and a fresh discovery replace them.
    let next = builder.refresh(&next, reading(Some(HealthState::NotFound), &["linear"]));
    assert_ne!(next.claude, previous.claude);
    assert!(matches!(next.claude, Some(Err(_))));
    assert_eq!(
        next.base_env.get("HOME").map(String::as_str),
        Some("/home/test")
    );
    assert_eq!(
        next.mcp_installed[&AgentKind::Claude],
        BTreeSet::from(["linear".to_string()])
    );
}

/// Task writes whose tree is not loaded until `ready` is set.
struct LateTree {
    inner: Arc<MemoryWrites>,
    ready: AtomicBool,
}

impl TaskWrites for LateTree {
    fn tree(&self, cx: &gpui::App) -> Option<Arc<WorkTree>> {
        if self.ready.load(Ordering::SeqCst) {
            self.inner.tree(cx)
        } else {
            None
        }
    }

    fn write(
        &self,
        ctx: Context,
        command: WriteCommand,
        cx: &mut gpui::App,
    ) -> gpui::Task<Result<Change, WriteError>> {
        self.inner.write(ctx, command, cx)
    }

    fn merge_is_ship(&self, task: &Task, cx: &gpui::App) -> bool {
        self.inner.merge_is_ship(task, cx)
    }
}

#[gpui::test]
fn the_runs_on_disk_load_once_at_the_first_work_tree(cx: &mut TestAppContext) {
    let world = World::new();
    let run = seed(&world, "single", mid_node);
    let linked = linked(&world, FakeRunner::new(), cx);
    let late = Arc::new(LateTree {
        inner: linked.writes.clone(),
        ready: AtomicBool::new(false),
    });
    let engine = linked.engine.with_task_writes(late.clone());
    cx.update(|cx| engine.attach(cx));
    cx.run_until_parked();

    advance(cx, 2);
    assert!(engine.lifecycle().all_runs().is_empty(), "no tree yet");

    late.ready.store(true, Ordering::SeqCst);
    advance(cx, 1);
    assert_eq!(engine.lifecycle().all_runs(), vec![run.clone()]);
    assert_eq!(engine.lifecycle().state(&run).unwrap().interrupted.len(), 1);

    advance(cx, 3);
    assert_eq!(engine.lifecycle().all_runs(), vec![run.clone()]);
    // It was loaded already: another load is refused as a duplicate.
    match engine.lifecycle().load_runs(&world.tree) {
        Err(crate::runtime::RuntimeError::Duplicate(id)) => assert_eq!(id, run),
        other => panic!("expected Duplicate, got {other:?}"),
    }
}
