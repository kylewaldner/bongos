//! Tests of the workflow step that runs checks: the real built-in on the fixture scripts, driven by
//! the workflow runtime with a stand-in for the agent runners. Time is the engine's fake clock; real
//! time only bounds how long a test waits for real processes.

use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bongos_workflow::process::ProcessRecord;
use bongos_workflow::run_view::{Mode, RunData, Snapshot};
use bongos_workflow::runtime::{Finish, RunSpec, Runner, Runtime};
use bongos_workflow::{BuiltinNodes, NodeStatusSources};
use bongos_workflow_model::record::EndState;
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{Definition, Failure, FailureReason, Limits, NodeKey, RunState, parse};
use gpui::{BorrowAppContext as _, TestAppContext};
use settings::SettingsStore;
use tempfile::TempDir;

use crate::engine_tests::{DRAIN_CHECKS, World, finish, live_keys, set_kill_grace, when_blocked};
use crate::pools::CheckPoolsRegistry;
use crate::pools_tests::{FakeClock, FakePool};
use crate::workflow_node::ChecksNode;

/// A checks node running `suite` (four parts, each needing the `gpu` pool): a pass ends the
/// run, a fail fails it, and no verdict is not routed, so it holds the run.
const VERIFY: &str = r#"
schema = 1
name = "verify"
start = "verify"

[nodes.verify]
kind = "program"
use = "checks"
with = { checks = ["suite"] }
routes = [
  { when = { verdict = "pass" }, to = "done" },
  { when = { verdict = "fail" }, to = "broken" },
]

[ends.done]
kind = "success"

[ends.broken]
kind = "failure"
reason = "checks failed"
"#;

/// One agent node.
const AGENT: &str = r#"
schema = 1
name = "agent"
start = "work"

[nodes.work]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

/// Stands in for the agent and program runners: starts are recorded and never finish.
#[derive(Default)]
struct Agents {
    started: Vec<(String, NodeKey)>,
    stopped: Vec<(String, NodeKey)>,
}

impl Runner for Agents {
    fn start(
        &mut self,
        run: &str,
        _: &Definition,
        key: &NodeKey,
        _: u32,
    ) -> Result<ProcessRecord, Failure> {
        self.started.push((run.to_string(), key.clone()));
        Ok(ProcessRecord {
            pid: 0,
            start_time: 0,
        })
    }

    fn poll(&mut self) -> Vec<Finish> {
        Vec::new()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        self.stopped.extend(tries.iter().cloned());
    }

    fn merge_branch(&mut self, _: &str, _: &str, _: &Branch) -> Result<String, Failure> {
        Err(Failure::new(
            FailureReason::MergeConflict,
            None,
            "no merges here",
        ))
    }
}

fn run_id(n: u32) -> String {
    format!("01890a5d-ac96-774b-bcce-b302{n:08x}")
}

fn spec(root: &Path, n: u32, text: &str) -> RunSpec {
    RunSpec {
        id: run_id(n),
        definition: parse(text).unwrap(),
        attempt_dir: root.join(format!("runs/{}/attempts/1", run_id(n))),
    }
}

fn verify_key() -> NodeKey {
    NodeKey::new("verify", 1)
}

fn limits(sessions: u32) -> Limits {
    Limits {
        max_concurrent_runs: 1,
        max_concurrent_sessions: Some(sessions),
        ..Limits::default()
    }
}

/// A runtime with the checks built-in of `world`'s engine registered, and the folder of its logs.
fn runtime(world: &World, cx: &mut TestAppContext, limits: Limits) -> (Runtime<Agents>, TempDir) {
    let builtins = cx.update(|cx| {
        ChecksNode::register(&world.engine, cx);
        BuiltinNodes::global(cx)
    });
    let mut runtime = Runtime::new(limits, Agents::default());
    runtime.set_builtins(builtins);
    (runtime, TempDir::new().unwrap())
}

fn no_reruns(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .task_checks
                    .get_or_insert_default()
                    .no_verdict_reruns = Some(0);
            });
        });
    });
}

/// Ticks the runtime and lets the app run until `until` holds, waiting for real processes.
fn drive<R: Runner>(
    cx: &mut TestAppContext,
    runtime: &mut Runtime<R>,
    mut until: impl FnMut(&mut Runtime<R>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        runtime.tick();
        cx.run_until_parked();
        runtime.tick();
        if until(runtime) {
            return;
        }
        assert!(Instant::now() < deadline, "the runtime never got there");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Lets the app and the parts run for a moment of real time, ticking the runtime.
fn settle<R: Runner>(cx: &mut TestAppContext, runtime: &mut Runtime<R>) {
    for _ in 0..10 {
        runtime.tick();
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn state(runtime: &Runtime<impl Runner>, n: u32) -> RunState {
    runtime.state(&run_id(n)).unwrap()
}

fn engine_idle(world: &World, cx: &mut TestAppContext) -> bool {
    !world
        .engine
        .read_with(cx, |engine, _| engine.is_running(crate::engine_tests::TASK))
}

/// Waits until the engine has no run of the task.
fn wait_idle(world: &World, cx: &mut TestAppContext) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !engine_idle(world, cx) {
        cx.run_until_parked();
        assert!(Instant::now() < deadline, "the checks never ended");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The pid of part `part` of the latest record of `suite`.
fn part_pid(world: &World, part: u32) -> i32 {
    let records = world.records("suite");
    let record = records.last().expect("a record of suite");
    let part = record
        .parts
        .iter()
        .find(|found| found.n == part)
        .expect("the part is recorded");
    part.pid.expect("the part's pid is recorded") as i32
}

/// Whether `pid` is a live process (a zombie counts as gone).
fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X"),
        Err(_) => false,
    }
}

fn wait_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[gpui::test]
async fn a_held_run_gets_no_command_holds_no_slot_and_resumes_with_its_kept_parts(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::automatic(), DRAIN_CHECKS);
    no_reruns(cx);
    let (mut runtime, logs) = runtime(&world, cx, limits(4));
    world.write("no-verdict-at", "3\n");
    runtime.add(spec(logs.path(), 1, VERIFY)).unwrap();

    drive(cx, &mut runtime, |runtime| {
        !state(runtime, 1).held.is_empty()
    });

    // The check gave no verdict at part 3 and nothing routes that: the run is held, not failed.
    let held = state(&runtime, 1);
    let failure = &held.held[&verify_key()];
    assert_eq!(failure.reason, FailureReason::CheckNoVerdict);
    assert_eq!(failure.detail, "suite: undeclared exit code 7");
    assert_eq!(held.ended, None);
    assert_eq!(world.lines("runs"), ["1", "2", "3", "4"]);
    // It gets no command: the log does not grow, nothing starts, and it holds no run slot (the
    // only one), so the next run starts.
    let events = runtime.events(&run_id(1)).unwrap().len();
    runtime.add(spec(logs.path(), 2, AGENT)).unwrap();
    settle(cx, &mut runtime);
    assert_eq!(runtime.events(&run_id(1)).unwrap().len(), events);
    assert!(runtime.in_flight(&run_id(1)).is_empty());
    assert_eq!(
        runtime.runner().started,
        [(run_id(2), NodeKey::new("work", 1))]
    );
    runtime.cancel(&run_id(2)).unwrap();

    // A resume reruns only the held key, from the parts that passed.
    world.forget("no-verdict-at");
    world.forget("runs");
    assert_eq!(runtime.resume(&run_id(1)).unwrap(), [verify_key()]);
    drive(cx, &mut runtime, |runtime| {
        state(runtime, 1).ended.is_some()
    });
    let ended = state(&runtime, 1);
    assert_eq!(ended.ended, Some((EndState::Succeeded, None)));
    assert!(ended.held.is_empty());
    assert_eq!(world.lines("runs"), ["3"], "parts 1, 2 and 4 were kept");
    let result = ended
        .last_finish(&verify_key())
        .unwrap()
        .result
        .clone()
        .unwrap();
    assert_eq!(result["verdict"], "pass");
    assert_eq!(result["failing"], serde_json::json!([]));
    let records = world.records("suite");
    assert_eq!(
        result["evidence"],
        serde_json::json!([records.last().unwrap().id])
    );
}

#[gpui::test]
async fn a_check_running_past_ten_minutes_is_not_cut(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let world = World::with_checks(cx, true, clock.clone(), DRAIN_CHECKS);
    let (mut runtime, logs) = runtime(&world, cx, limits(4));
    world.write("block-at", "1\n");
    let blocked = when_blocked(&world, 1);
    runtime.add(spec(logs.path(), 1, VERIFY)).unwrap();
    drive(cx, &mut runtime, |runtime| {
        !runtime.in_flight(&run_id(1)).is_empty()
    });
    let mut blocked = blocked;
    let deadline = Instant::now() + Duration::from_secs(60);
    while futures::FutureExt::now_or_never(&mut blocked).is_none() {
        cx.run_until_parked();
        assert!(Instant::now() < deadline, "part 1 never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Eleven minutes pass on the engine's clock, past the program cap of ten.
    clock.advance(Duration::from_secs(11 * 60));
    settle(cx, &mut runtime);

    let running = state(&runtime, 1);
    assert_eq!(running.finished, []);
    assert!(running.running.contains_key(&verify_key()));
    assert_eq!(runtime.in_flight(&run_id(1)), [verify_key()]);
    assert!(!engine_idle(&world, cx));
    let records = world.records("suite");
    assert!(records[0].parts[0].ended.is_none(), "part 1 still runs");
    let pid = part_pid(&world, 1);
    assert!(alive(pid));
    // The key never went to the agent and program runner.
    assert!(runtime.runner().started.is_empty());

    // A cancel stops it, and the stopped try reports nothing.
    runtime.cancel(&run_id(1)).unwrap();
    wait_idle(&world, cx);
    wait_gone(pid);
    settle(cx, &mut runtime);
    assert_eq!(state(&runtime, 1).finished, []);
}

#[gpui::test]
async fn a_check_key_holds_no_session_slot(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::automatic(), DRAIN_CHECKS);
    // One session in all, two runs at once.
    let (mut runtime, logs) = runtime(
        &world,
        cx,
        Limits {
            max_concurrent_runs: 2,
            ..limits(1)
        },
    );
    world.write("block-at", "1\n");
    runtime.add(spec(logs.path(), 1, VERIFY)).unwrap();
    runtime.add(spec(logs.path(), 2, AGENT)).unwrap();

    drive(cx, &mut runtime, |runtime| {
        !runtime.in_flight(&run_id(1)).is_empty() && !runtime.runner().started.is_empty()
    });

    // The agent took the only session while the check runs.
    assert_eq!(
        runtime.runner().started,
        [(run_id(2), NodeKey::new("work", 1))]
    );
    assert_eq!(runtime.in_flight(&run_id(1)), [verify_key()]);
    assert_eq!(runtime.sessions_in_flight(), 1);

    runtime.cancel(&run_id(1)).unwrap();
    runtime.cancel(&run_id(2)).unwrap();
    wait_idle(&world, cx);
}

#[gpui::test]
async fn the_run_view_line_shows_the_pool_wait_and_the_part_progress(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let clock = FakeClock::manual();
    let world = World::with_checks(cx, true, clock.clone(), DRAIN_CHECKS);
    let pool = FakePool::new("gpu", 1, clock.clone());
    cx.update(|cx| CheckPoolsRegistry::set(Rc::new(pool.clone()), cx));
    let held = pool.hold("another task", 1);
    let (mut runtime, logs) = runtime(&world, cx, limits(4));
    world.write("block-at", "3\n");
    runtime.add(spec(logs.path(), 1, VERIFY)).unwrap();
    drive(cx, &mut runtime, |_| pool.queued() == 1);

    let line = |runtime: &Runtime<Agents>, cx: &mut TestAppContext| {
        let definition = runtime.definition(&run_id(1)).unwrap().clone();
        let run_state = state(runtime, 1);
        let in_flight = runtime.in_flight(&run_id(1));
        cx.update(|cx| {
            let data = RunData {
                run: run_id(1),
                workflow: "verify".to_string(),
                layer: "profile".to_string(),
                todo: "Backfill".to_string(),
                node_status: NodeStatusSources::running(&definition, &run_id(1), &in_flight, cx),
                definition,
                state: run_state,
                worktree: None,
                attempt_dir: None,
                session_limit: 4,
            };
            Snapshot::build(&data, None).text(Mode::List)
        })
    };

    // Waiting for the pool, in its line.
    let deadline = Instant::now() + Duration::from_secs(30);
    let text = loop {
        settle(cx, &mut runtime);
        let text = line(&runtime, cx);
        if text.contains("waiting") {
            break text;
        }
        assert!(Instant::now() < deadline, "{text}");
    };
    assert!(
        text.contains("\n    suite: waiting: gpu, place 1, "),
        "{text}"
    );
    assert!(text.contains("sessions 0 of 4"), "{text}");

    // The pool frees; parts 1 and 2 pass and part 3 runs.
    let blocked = when_blocked(&world, 3);
    drop(held);
    drive(cx, &mut runtime, |_| {
        world.root().join("tree/blocked-3").exists()
    });
    drop(blocked);
    let deadline = Instant::now() + Duration::from_secs(30);
    let text = loop {
        clock.advance(Duration::from_secs(2));
        settle(cx, &mut runtime);
        let text = line(&runtime, cx);
        if text.contains("running part 3") {
            break text;
        }
        assert!(Instant::now() < deadline, "{text}");
    };
    assert!(
        text.contains("\n    suite: running part 3 of 4, 2 kept\n"),
        "{text}"
    );

    runtime.cancel(&run_id(1)).unwrap();
    wait_idle(&world, cx);
    let text = line(&runtime, cx);
    assert!(!text.contains("suite: "), "{text}");
}

#[cfg(target_os = "linux")]
#[gpui::test]
async fn a_crash_during_a_part_leaves_the_run_interrupted_and_its_process_killed_at_load(
    cx: &mut TestAppContext,
) {
    use bongos_checks_model::verdict::{NoVerdictReason, Verdict};
    use bongos_workflow_model::Event;

    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::automatic(), DRAIN_CHECKS);
    // The first bongos would otherwise run the killed part again by itself.
    no_reruns(cx);
    let (mut crashed, logs) = runtime(&world, cx, limits(4));
    world.write("block-at", "3\n");
    let blocked = when_blocked(&world, 3);
    crashed.add(spec(logs.path(), 1, VERIFY)).unwrap();
    drive(cx, &mut crashed, |_| {
        world.root().join("tree/blocked-3").exists()
    });
    drop(blocked);
    cx.run_until_parked();
    let pid = part_pid(&world, 3);
    assert!(alive(pid));
    // The crash: this runtime is gone without a quit, and its part keeps running.
    drop(crashed);

    // The relaunched bongos has its own engine and loads the run from its log.
    let engine = world.second_engine(cx);
    let (node, _) = cx.update(|cx| ChecksNode::new(&engine, cx));
    let builtins = BuiltinNodes::default();
    builtins.insert("checks", node);
    let mut loaded = Runtime::new(limits(4), Agents::default());
    loaded.set_builtins(builtins);
    loaded.load(spec(logs.path(), 1, VERIFY)).unwrap();
    cx.run_until_parked();

    let interrupted = state(&loaded, 1);
    assert!(interrupted.interrupted.contains_key(&verify_key()));
    assert!(interrupted.running.is_empty());
    assert!(matches!(
        loaded.events(&run_id(1)).and_then(|events| events.last()),
        Some(Event::Interrupted { keys }) if *keys == [verify_key()]
    ));
    // The part's process is stopped at load, and the run gets no command.
    wait_gone(pid);
    settle(cx, &mut loaded);
    assert!(loaded.in_flight(&run_id(1)).is_empty());
    assert!(loaded.runner().started.is_empty());
    // Parts 1 and 2 finished and stay recorded; part 3 ended by the load's SIGTERM, long before
    // its minute of sleep was over.
    wait_idle(&world, cx);
    let records = world.records("suite");
    let parts: Vec<(u32, Verdict)> = records[0]
        .parts
        .iter()
        .take(3)
        .map(|part| (part.n, part.verdict.clone()))
        .collect();
    assert_eq!(
        parts,
        [
            (1, Verdict::Pass),
            (2, Verdict::Pass),
            (
                3,
                Verdict::NoVerdict(NoVerdictReason::Killed { signal: 15 })
            ),
        ]
    );
}

#[gpui::test]
async fn a_drain_during_a_checks_key_returns_its_run_and_key_and_stops_the_part_keeping_the_rest(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::manual(), DRAIN_CHECKS);
    set_kill_grace(cx, 3);
    let (mut runtime, logs) = runtime(&world, cx, limits(4));
    world.write("block-at", "3\n");
    runtime.add(spec(logs.path(), 1, VERIFY)).unwrap();
    drive(cx, &mut runtime, |_| {
        world.root().join("tree/blocked-3").exists()
    });
    cx.run_until_parked();
    let pid = part_pid(&world, 3);

    let drain = world.engine.update(cx, |engine, cx| engine.drain(cx));
    let drained = finish(cx, drain);

    assert_eq!(drained, [(run_id(1), verify_key())]);
    wait_idle(&world, cx);
    wait_gone(pid);
    // Parts 1 and 2 finished and stay in the unfinished record; part 3 was stopped; part 4 never
    // started.
    let records = world.records("suite");
    assert_eq!(records.len(), 1);
    let parts: Vec<(u32, bool)> = records[0]
        .parts
        .iter()
        .map(|part| (part.n, part.verdict.is_pass()))
        .collect();
    assert_eq!(parts, [(1, true), (2, true), (3, false)]);
    assert_eq!(world.lines("runs"), ["1", "2", "3"]);
    assert!(live_keys(&world).is_empty());
    // The key is handed over: the run records no finish for it and it is not held.
    settle(cx, &mut runtime);
    let after = state(&runtime, 1);
    assert_eq!(after.finished, []);
    assert!(after.held.is_empty());
    assert_eq!(runtime.in_flight(&run_id(1)), [verify_key()]);
    runtime.quit();
}

#[cfg(target_os = "linux")]
#[gpui::test]
async fn a_crash_during_a_part_has_its_process_killed_when_the_relaunch_follows_the_project(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::automatic(), DRAIN_CHECKS);
    no_reruns(cx);
    let (mut crashed, logs) = runtime(&world, cx, limits(4));
    world.write("block-at", "3\n");
    let blocked = when_blocked(&world, 3);
    crashed.add(spec(logs.path(), 1, VERIFY)).unwrap();
    drive(cx, &mut crashed, |_| {
        world.root().join("tree/blocked-3").exists()
    });
    drop(blocked);
    cx.run_until_parked();
    let pid = part_pid(&world, 3);
    assert!(alive(pid));
    // The crash: the runtime is gone without a quit, and its part keeps running.
    drop(crashed);

    // The relaunched bongos loads no run at all; its engine follows the project's store at
    // load, as the decider does for every open project, and that stops the part.
    let engine = world.second_engine(cx);
    engine.update(cx, |engine, cx| engine.follow_project(&world.repo, cx));
    cx.run_until_parked();
    wait_gone(pid);
    wait_idle(&world, cx);
    let records = world.records("suite");
    assert_eq!(
        records[0].parts[2].verdict,
        bongos_checks_model::verdict::Verdict::NoVerdict(
            bongos_checks_model::verdict::NoVerdictReason::Killed { signal: 15 }
        )
    );
}

#[gpui::test]
async fn an_undrain_starts_the_drained_checks_key_again_with_no_other_event(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let world = World::with_checks(cx, true, FakeClock::manual(), DRAIN_CHECKS);
    set_kill_grace(cx, 3);
    let (mut runtime, logs) = runtime(&world, cx, limits(4));
    world.write("block-at", "3\n");
    runtime.add(spec(logs.path(), 1, VERIFY)).unwrap();
    drive(cx, &mut runtime, |_| {
        world.root().join("tree/blocked-3").exists()
    });
    cx.run_until_parked();
    let drain = world.engine.update(cx, |engine, cx| engine.drain(cx));
    assert_eq!(finish(cx, drain), [(run_id(1), verify_key())]);
    wait_idle(&world, cx);
    settle(cx, &mut runtime);
    // The key waits for the drain to be abandoned: nothing of it runs.
    assert!(engine_idle(&world, cx));
    assert_eq!(runtime.in_flight(&run_id(1)), [verify_key()]);
    world.forget("block-at");
    world.forget("runs");

    // The undrain alone wakes the key: it starts again from the kept parts and passes.
    world.engine.update(cx, |engine, cx| engine.undrain(cx));
    drive(cx, &mut runtime, |runtime| {
        state(runtime, 1).ended.is_some()
    });
    assert_eq!(state(&runtime, 1).ended, Some((EndState::Succeeded, None)));
    assert_eq!(world.lines("runs"), ["3", "4"], "parts 1 and 2 were kept");
    assert!(runtime.runner().started.is_empty());
}
