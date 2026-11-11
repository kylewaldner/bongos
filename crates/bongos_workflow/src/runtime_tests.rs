//! Tests of the engine runtime: the run and session limits, Queued, halt with a child that ignores
//! SIGTERM, quit and relaunch as interrupted, and the orphan kill at load. Logs live in temporary
//! directories; real processes are `sh` and `sleep` in their own sessions.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bongos_workflow_model::record::{EndState, Lifecycle};
use bongos_workflow_model::{Definition, Event, Limits, NodeKey, parse};
use serde_json::json;
use tempfile::TempDir;

use crate::process::{self, NodeProcess, OrphanOutcome, ProcessRecord, STOP_GRACE};
use crate::runtime::fake::FakeRunner;
use crate::runtime::{EVENTS_FILE, RunSpec, Runtime};

const SINGLE: &str = r#"
schema = 1
name = "single"
start = "work"

[nodes.work]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const FAN: &str = r#"
schema = 1
name = "fan"
start = "split"

[nodes.split]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = ["a", "b", "c"] }]

[nodes.a]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.b]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.c]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.gather]
kind = "agent"
join = { wait = "all" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const GATED: &str = r#"
schema = 1
name = "gated"
start = "work"

[nodes.work]
kind = "agent"
gate = { when = "before", question = "Go ahead?" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

fn definition(text: &str) -> Definition {
    parse(text).unwrap()
}

fn run_id(n: u32) -> String {
    format!("01890a5d-ac96-774b-bcce-b302{n:08x}")
}

fn spec(root: &Path, n: u32, text: &str) -> RunSpec {
    RunSpec {
        id: run_id(n),
        definition: definition(text),
        attempt_dir: root.join(format!("runs/{}/attempts/1", run_id(n))),
    }
}

fn runtime(runner: FakeRunner) -> Runtime<FakeRunner> {
    Runtime::new(Limits::default(), runner)
}

fn lifecycle(runtime: &Runtime<FakeRunner>, n: u32) -> Lifecycle {
    runtime.state(&run_id(n)).unwrap().lifecycle()
}

fn key(node: &str) -> NodeKey {
    NodeKey::new(node, 1)
}

fn item(node: &str, index: u32) -> NodeKey {
    NodeKey::new(node, 1).in_branch("split", index)
}

/// Waits until `condition` holds, failing after `limit`.
fn wait_until(limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out after {limit:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A command that ignores SIGTERM, then writes `ready` and sleeps: only SIGKILL ends it.
fn stubborn_command(ready: &Path) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        "trap '' TERM; : > \"$1\"; exec sleep 60".to_string(),
        "sh".to_string(),
        ready.to_string_lossy().into_owned(),
    ]
}

fn sleeper() -> NodeProcess {
    let mut command = Command::new("sleep");
    command.arg("60");
    NodeProcess::spawn(command, Stdio::null(), Stdio::null(), Stdio::null()).unwrap()
}

#[test]
fn four_runs_with_a_limit_of_three_queue_one() {
    let temp = TempDir::new().unwrap();
    let mut runtime = runtime(FakeRunner::new());
    for n in 1..=4 {
        runtime.add(spec(temp.path(), n, SINGLE)).unwrap();
    }
    runtime.tick();

    let started: Vec<String> = runtime
        .runner()
        .started
        .iter()
        .map(|(run, _)| run.clone())
        .collect();
    assert_eq!(started, vec![run_id(1), run_id(2), run_id(3)]);
    for n in 1..=3 {
        assert_eq!(lifecycle(&runtime, n), Lifecycle::Running);
    }
    assert_eq!(lifecycle(&runtime, 4), Lifecycle::Queued);
    assert!(runtime.is_queued(&run_id(4)));
    assert_eq!(
        runtime.events(&run_id(4)).unwrap(),
        &[Event::Created, Event::Queued]
    );
    // The Queued event is on disk too.
    let log = std::fs::read_to_string(spec(temp.path(), 4, SINGLE).attempt_dir.join(EVENTS_FILE))
        .unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");

    // A run that ends frees its slot for the queued one, and only then does it start.
    runtime.runner_mut().release(&run_id(2));
    runtime.tick();
    assert_eq!(
        lifecycle(&runtime, 2),
        Lifecycle::Ended(EndState::Succeeded)
    );
    assert_eq!(lifecycle(&runtime, 4), Lifecycle::Running);
    assert!(!runtime.is_queued(&run_id(4)));
    assert_eq!(runtime.runner().started.len(), 4);
    assert_eq!(runtime.runner().started[3].0, run_id(4));

    runtime.runner_mut().auto_finish = true;
    runtime.tick();
    for n in 1..=4 {
        assert_eq!(
            lifecycle(&runtime, n),
            Lifecycle::Ended(EndState::Succeeded)
        );
    }
}

#[test]
fn sessions_are_capped_at_six_across_runs() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    let mut runtime = runtime(runner);
    assert_eq!(runtime.limits().session_limit(), 6);
    for n in 1..=3 {
        runtime.add(spec(temp.path(), n, FAN)).unwrap();
    }

    // Each run's split starts; once they finish, three runs want three branches each.
    runtime.tick();
    assert_eq!(runtime.sessions_in_flight(), 3);
    runtime.tick();
    assert_eq!(runtime.sessions_in_flight(), 6);
    assert_eq!(runtime.runner().in_flight().len(), 6);

    for _ in 0..20 {
        runtime.tick();
    }
    assert_eq!(runtime.runner().max_in_flight, 6);
    for n in 1..=3 {
        assert_eq!(
            lifecycle(&runtime, n),
            Lifecycle::Ended(EndState::Succeeded)
        );
    }
    // Every branch of every run ran.
    assert_eq!(runtime.runner().started.len(), 3 * 5);
}

#[test]
fn halt_ends_a_child_that_ignores_sigterm_within_five_seconds() {
    let temp = TempDir::new().unwrap();
    let ready = temp.path().join("ready");
    let mut runner = FakeRunner::new();
    runner.command = Some(stubborn_command(&ready));
    let mut runtime = runtime(runner);
    runtime.add(spec(temp.path(), 1, SINGLE)).unwrap();
    runtime.tick();
    wait_until(Duration::from_secs(5), || ready.exists());

    let record = runtime.runner_mut().processes().next().unwrap().record();
    assert_ne!(record.start_time, 0);
    let state = runtime.state(&run_id(1)).unwrap();
    let running = &state.running[&key("work")];
    assert_eq!(
        (running.pid, running.pid_start_time),
        (record.pid, record.start_time)
    );

    let started = Instant::now();
    runtime.halt(&run_id(1)).unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "halt took {elapsed:?}");
    // It ignored SIGTERM, so only SIGKILL after the grace ended it.
    assert!(elapsed >= STOP_GRACE, "halt took {elapsed:?}");
    let runner = runtime.runner_mut();
    assert_eq!(runner.stopped, vec![(run_id(1), key("work"))]);
    assert!(runner.stopped_processes[0].has_exited());
    assert_ne!(process::start_time(record.pid), Some(record.start_time));

    let state = runtime.state(&run_id(1)).unwrap();
    assert_eq!(state.lifecycle(), Lifecycle::Halted);
    // The halted key stays recorded running, so a resume reruns it; nothing else starts.
    assert!(state.running.contains_key(&key("work")));
    runtime.tick();
    assert_eq!(runtime.runner().started.len(), 1);
}

#[test]
fn cancel_stops_processes_and_closes_the_run() {
    let temp = TempDir::new().unwrap();
    let ready = temp.path().join("ready");
    let mut runner = FakeRunner::new();
    runner.command = Some(stubborn_command(&ready));
    let mut runtime = runtime(runner);
    runtime.add(spec(temp.path(), 1, SINGLE)).unwrap();
    runtime.tick();
    wait_until(Duration::from_secs(5), || ready.exists());

    let started = Instant::now();
    runtime.cancel(&run_id(1)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(runtime.runner_mut().stopped_processes[0].has_exited());
    assert_eq!(lifecycle(&runtime, 1), Lifecycle::Cancelled);
    assert!(runtime.cancel(&run_id(1)).is_err());
}

#[test]
fn quit_then_reload_shows_interrupted_and_starts_nothing() {
    let temp = TempDir::new().unwrap();
    let mut runtime = runtime(FakeRunner::new());
    runtime.add(spec(temp.path(), 1, FAN)).unwrap();
    runtime.add(spec(temp.path(), 2, SINGLE)).unwrap();
    runtime.tick();
    runtime.runner_mut().release(&run_id(1));
    runtime.tick();
    let in_flight: BTreeSet<NodeKey> = [item("a", 0), item("b", 1), item("c", 2)].into();
    assert_eq!(
        runtime
            .state(&run_id(1))
            .unwrap()
            .running
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        in_flight
    );

    runtime.quit();
    assert_eq!(runtime.runner().stopped.len(), 4);
    assert!(!runtime.tick());
    drop(runtime);

    let mut relaunched = Runtime::new(Limits::default(), FakeRunner::new());
    relaunched.load(spec(temp.path(), 1, FAN)).unwrap();
    relaunched.load(spec(temp.path(), 2, SINGLE)).unwrap();
    relaunched.tick();
    assert!(relaunched.runner().started.is_empty());
    let first = relaunched.state(&run_id(1)).unwrap();
    assert_eq!(
        first.interrupted.keys().cloned().collect::<BTreeSet<_>>(),
        in_flight
    );
    assert!(first.running.is_empty());
    assert_eq!(
        relaunched
            .state(&run_id(2))
            .unwrap()
            .interrupted
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![key("work")]
    );
    // Loading appended nothing: the quit already recorded the interruption.
    let interrupted = relaunched
        .events(&run_id(1))
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::Interrupted { .. }))
        .count();
    assert_eq!(interrupted, 1);
}

#[test]
fn reload_after_a_crash_marks_every_started_key_interrupted() {
    let temp = TempDir::new().unwrap();
    let mut runtime = runtime(FakeRunner::new());
    runtime.add(spec(temp.path(), 1, FAN)).unwrap();
    runtime.tick();
    runtime.runner_mut().release_all();
    runtime.tick();
    // A crash: no quit, the runtime is gone with its tries recorded as running.
    drop(runtime);

    let mut relaunched = Runtime::new(Limits::default(), FakeRunner::new());
    relaunched.load(spec(temp.path(), 1, FAN)).unwrap();
    relaunched.tick();
    assert!(relaunched.runner().started.is_empty());
    let state = relaunched.state(&run_id(1)).unwrap();
    assert_eq!(
        state.interrupted.keys().cloned().collect::<BTreeSet<_>>(),
        [item("a", 0), item("b", 1), item("c", 2)].into()
    );
    assert!(matches!(
        relaunched.events(&run_id(1)).unwrap().last(),
        Some(Event::Interrupted { keys }) if keys.len() == 3
    ));
}

#[test]
fn a_gate_holds_its_branch_and_no_session() {
    let temp = TempDir::new().unwrap();
    let mut runtime = runtime(FakeRunner::new());
    runtime.add(spec(temp.path(), 1, GATED)).unwrap();
    runtime.tick();
    assert!(runtime.runner().started.is_empty());
    assert_eq!(runtime.sessions_in_flight(), 0);
    let state = runtime.state(&run_id(1)).unwrap();
    assert_eq!(state.open_gates.get(&key("work")).unwrap(), "Go ahead?");
    // Stepping again opens nothing twice.
    runtime.tick();
    let opened = runtime
        .events(&run_id(1))
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::GateOpened { .. }))
        .count();
    assert_eq!(opened, 1);
}

#[test]
fn a_leftover_process_is_killed_only_when_its_start_time_matches() {
    let temp = TempDir::new().unwrap();
    let mut matching = sleeper();
    let mut other = sleeper();
    wait_until(Duration::from_secs(5), || {
        process::start_time(matching.record().pid).is_some()
            && process::start_time(other.record().pid).is_some()
    });
    let matching_record = matching.record();
    let other_record = other.record();
    assert_ne!(matching_record.start_time, 0);

    // The log of a run that crashed with two tries running: one recorded with the live
    // process's start time, one whose pid now belongs to a process started at another time.
    let mut log = Event::Created.to_line();
    log.push_str(
        &Event::NodeStarted {
            key: item("a", 0),
            try_number: 1,
            pid: matching_record.pid,
            pid_start_time: matching_record.start_time,
        }
        .to_line(),
    );
    log.push_str(
        &Event::NodeStarted {
            key: item("b", 1),
            try_number: 1,
            pid: other_record.pid,
            pid_start_time: other_record.start_time + 1,
        }
        .to_line(),
    );
    let run = spec(temp.path(), 1, FAN);
    std::fs::create_dir_all(&run.attempt_dir).unwrap();
    std::fs::write(run.attempt_dir.join(EVENTS_FILE), log).unwrap();

    let mut relaunched = Runtime::new(Limits::default(), FakeRunner::new());
    let orphans = relaunched.load(run).unwrap();
    assert_eq!(
        orphans,
        vec![
            (item("a", 0), OrphanOutcome::Killed),
            (item("b", 1), OrphanOutcome::NotRunning),
        ]
    );
    wait_until(Duration::from_secs(5), || matching.has_exited());
    assert!(
        !other.has_exited(),
        "a different start time is never killed"
    );

    let state = relaunched.state(&run_id(1)).unwrap();
    assert_eq!(state.interrupted.len(), 2);
    relaunched.tick();
    assert!(relaunched.runner().started.is_empty());

    other.stop();
    assert!(other.has_exited());
}

#[test]
fn an_unknown_start_time_is_never_killed() {
    let mut child = sleeper();
    let record = ProcessRecord {
        pid: child.record().pid,
        start_time: 0,
    };
    assert_eq!(process::kill_orphan(record), OrphanOutcome::Unknown);
    assert!(!child.has_exited());
    child.stop();
}

#[test]
fn scripted_results_route() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    runner.script("work", json!({ "summary": "done" }));
    let mut runtime = runtime(runner);
    runtime.add(spec(temp.path(), 1, SINGLE)).unwrap();
    runtime.tick();
    runtime.tick();
    let state = runtime.state(&run_id(1)).unwrap();
    assert_eq!(state.ended, Some((EndState::Succeeded, None)));
    assert_eq!(
        state.last_finish(&key("work")).unwrap().result,
        Some(json!({ "summary": "done" }))
    );
}
