//! Tests of the engine runtime: the run and session limits, Queued, halt with a child that ignores
//! SIGTERM, quit and relaunch as interrupted, and the orphan kill at load. Logs live in temporary
//! directories; real processes are `sh` and `sleep` in their own sessions.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bongos_workflow_model::record::{CapReached, EndState, Lifecycle};
use bongos_workflow_model::{Definition, Event, FailureReason, Limits, NodeKey, parse, replay};
use serde_json::json;
use tempfile::TempDir;

use crate::process::{self, NodeProcess, OrphanOutcome, ProcessRecord, STOP_GRACE};
use crate::runtime::fake::FakeRunner;
use crate::runtime::{EVENTS_FILE, RunSpec, Runtime, RuntimeError};

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

/// Two checks run side by side in a fan-out, neither with a route for `no_verdict`.
const CHECKED_FAN: &str = r#"
schema = 1
name = "checkedfan"
start = "split"

[nodes.split]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = ["lint", "test"] }]

[nodes.lint]
kind = "program"
use = "checks"
routes = [
  { when = { verdict = "pass" }, to = "gather" },
  { when = { verdict = "fail" }, to = "broken" },
]

[nodes.test]
kind = "program"
use = "checks"
routes = [
  { when = { verdict = "pass" }, to = "gather" },
  { when = { verdict = "fail" }, to = "broken" },
]

[nodes.gather]
kind = "agent"
join = { wait = "all" }
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"

[ends.broken]
kind = "failure"
reason = "checks failed"
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
        loop_bound: 5,
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
    let orphans = relaunched
        .load_all(vec![
            spec(temp.path(), 1, FAN),
            spec(temp.path(), 2, SINGLE),
        ])
        .unwrap();
    assert_eq!(orphans.len(), 2, "one outcome list per run");
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

#[test]
fn a_merge_conflict_fails_the_join_and_starts_nothing_after() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    runner.merge_conflicts = true;
    let mut runtime = runtime(runner);
    runtime.add(spec(temp.path(), 1, FAN)).unwrap();
    for _ in 0..5 {
        runtime.tick();
    }

    let state = runtime.state(&run_id(1)).unwrap();
    let (end, failure) = state.ended.clone().unwrap();
    assert_eq!(end, EndState::Failed);
    let failure = failure.unwrap();
    assert_eq!(failure.reason, FailureReason::MergeConflict);
    assert_eq!(failure.node, Some(key("gather")));
    // The first conflict stopped the step: no other merge was tried and the join never ran.
    let runner = runtime.runner();
    assert_eq!(runner.merges.len(), 1);
    let started: Vec<&str> = runner
        .started
        .iter()
        .map(|(_, key)| key.node.as_str())
        .collect();
    assert_eq!(started, vec!["split", "a", "b", "c"]);
    // The conflict is recorded as a failed try of the join's key, with no process.
    let finish = state.last_finish(&key("gather")).unwrap();
    assert_eq!(
        finish.failure.as_ref().unwrap().reason,
        FailureReason::MergeConflict
    );
    assert!(finish.had_start);
    assert!(state.running.is_empty());
}

#[test]
fn a_merge_conflict_keeps_the_branches_merged_before_it() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    runner.merge_conflicts = true;
    runner.clean_merges = 1;
    let mut runtime = runtime(runner);
    runtime.add(spec(temp.path(), 1, FAN)).unwrap();
    for _ in 0..5 {
        runtime.tick();
    }

    let state = runtime.state(&run_id(1)).unwrap();
    let (end, failure) = state.ended.unwrap();
    assert_eq!(end, EndState::Failed);
    assert_eq!(failure.unwrap().reason, FailureReason::MergeConflict);
    // The first branch was merged into the run branch before the second conflicted, and the log
    // says so; the conflict stopped the third merge.
    let merged: Vec<_> = runtime
        .events(&run_id(1))
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            Event::BranchMerged { join, branch, .. } => Some((join.clone(), branch.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].0, "gather");
    assert_eq!(runtime.runner().merges.len(), 2);
    assert_eq!(merged[0].1, runtime.runner().merges[0].2);
}

#[test]
fn runs_waiting_at_gates_hold_no_run_slot() {
    let temp = TempDir::new().unwrap();
    let mut runtime = runtime(FakeRunner::new());
    for n in 1..=3 {
        runtime.add(spec(temp.path(), n, GATED)).unwrap();
    }
    runtime.add(spec(temp.path(), 4, SINGLE)).unwrap();
    runtime.tick();

    for n in 1..=3 {
        let state = runtime.state(&run_id(n)).unwrap();
        assert!(state.open_gates.contains_key(&key("work")));
    }
    // The three gated runs freed their slots, so the fourth started.
    assert_eq!(runtime.runner().started, vec![(run_id(4), key("work"))]);
    assert_eq!(lifecycle(&runtime, 4), Lifecycle::Running);
    assert!(!runtime.is_queued(&run_id(4)));

    // A fifth run starts too while the gated runs wait.
    runtime.add(spec(temp.path(), 5, SINGLE)).unwrap();
    runtime.tick();
    assert_eq!(runtime.runner().started.len(), 2);
    assert_eq!(runtime.runner().started[1].0, run_id(5));
}

/// A leader that exits on SIGTERM after starting a child that ignores it and stays in the group;
/// the child's pid is written to `$1`.
fn leader_leaving_a_child(pid_file: &Path) -> Command {
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "(trap '' TERM; exec sleep 60) & echo $! > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"; wait",
        "sh",
    ]);
    command.arg(pid_file);
    command
}

fn spawn_leader_leaving_a_child(temp: &TempDir) -> (NodeProcess, u32) {
    let pid_file = temp.path().join("child");
    let leader = NodeProcess::spawn(
        leader_leaving_a_child(&pid_file),
        Stdio::null(),
        Stdio::null(),
        Stdio::null(),
    )
    .unwrap();
    wait_until(Duration::from_secs(5), || pid_file.exists());
    let child: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_until(Duration::from_secs(5), || {
        process::start_time(leader.record().pid).is_some()
    });
    (leader, child)
}

#[test]
fn stop_kills_a_child_left_in_the_group_when_the_leader_exits_on_sigterm() {
    let temp = TempDir::new().unwrap();
    let (mut leader, child) = spawn_leader_leaving_a_child(&temp);
    let group = leader.record().pid;
    assert!(process::group_alive(group));
    assert!(process::start_time(child).is_some());

    let started = Instant::now();
    leader.stop();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(5), "stop took {elapsed:?}");
    // The child ignored SIGTERM, so it lived through the grace and only SIGKILL ended it.
    assert!(elapsed >= STOP_GRACE, "stop took {elapsed:?}");
    assert!(leader.has_exited());
    assert!(!process::group_alive(group), "the child outlived the stop");
}

#[test]
fn an_orphan_group_is_killed_even_when_its_leader_exits_on_sigterm() {
    let temp = TempDir::new().unwrap();
    let (mut leader, _child) = spawn_leader_leaving_a_child(&temp);
    let mut sleeping = sleeper();
    wait_until(Duration::from_secs(5), || {
        process::start_time(sleeping.record().pid).is_some()
    });
    let records = [leader.record(), sleeping.record()];

    let started = Instant::now();
    let outcomes = process::kill_orphans(&records);
    let elapsed = started.elapsed();
    assert_eq!(outcomes, vec![OrphanOutcome::Killed, OrphanOutcome::Killed]);
    // Both share one grace period.
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    assert!(!process::group_alive(records[0].pid));
    assert!(!process::group_alive(records[1].pid));
    assert!(leader.has_exited());
    assert!(sleeping.has_exited());
}

fn check_result(verdict: &str) -> serde_json::Value {
    json!({
        "verdict": verdict,
        "failing": [],
        "scope": "changed files",
        "evidence": [],
        "summary": "",
    })
}

fn held_nodes(runtime: &Runtime<FakeRunner>) -> Vec<String> {
    let state = runtime.state(&run_id(1)).unwrap();
    state.held.keys().map(|key| key.node.clone()).collect()
}

#[test]
fn checks_without_a_verdict_in_two_branches_hold_the_run_one_at_a_time() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    runner
        .script("lint", check_result("no_verdict"))
        .script("test", check_result("no_verdict"));
    let mut runtime = runtime(runner);
    runtime.add(spec(temp.path(), 1, CHECKED_FAN)).unwrap();
    for _ in 0..5 {
        runtime.tick();
    }
    // Both checks came up without a verdict; the run is held on the first.
    assert_eq!(held_nodes(&runtime), ["lint"]);
    assert_eq!(runtime.state(&run_id(1)).unwrap().ended, None);
    assert_eq!(
        runtime.runner().started.len(),
        3,
        "a held run starts nothing"
    );

    // A resume reruns only the held check; the other's recorded result then holds the run.
    runtime.runner_mut().script("lint", check_result("pass"));
    assert_eq!(runtime.resume(&run_id(1)).unwrap().len(), 1);
    for _ in 0..5 {
        runtime.tick();
    }
    assert_eq!(held_nodes(&runtime), ["test"]);
    assert_eq!(runtime.runner().started.len(), 4);

    runtime.runner_mut().script("test", check_result("pass"));
    assert_eq!(runtime.resume(&run_id(1)).unwrap().len(), 1);
    for _ in 0..5 {
        runtime.tick();
    }
    let state = runtime.state(&run_id(1)).unwrap();
    assert_eq!(state.ended, Some((EndState::Succeeded, None)));
    assert!(state.held.is_empty());
    let started: Vec<&str> = runtime
        .runner()
        .started
        .iter()
        .map(|(_, key)| key.node.as_str())
        .collect();
    assert_eq!(started, ["split", "lint", "test", "lint", "test", "gather"]);
}

const UNBOUNDED_LOOP: &str = r#"
schema = 1
name = "loop"
start = "work"

[nodes.work]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "again" }]

[nodes.again]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "work" }]

[ends.done]
kind = "success"
"#;

#[test]
fn a_loaded_attempt_keeps_its_recorded_loop_bound() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    runner.script("work", json!({ "summary": "w" }));
    runner.script("again", json!({ "summary": "a" }));
    let mut runtime = runtime(runner);
    // The settings now say 9; the attempt recorded 2.
    runtime.set_limits(Limits {
        loop_bound: 9,
        ..Limits::default()
    });
    let mut spec = spec(temp.path(), 1, UNBOUNDED_LOOP);
    spec.loop_bound = 2;
    runtime.add(spec).unwrap();
    for _ in 0..20 {
        runtime.tick();
    }
    let state = runtime.state(&run_id(1)).unwrap();
    let (end, failure) = state.ended.unwrap();
    assert_eq!(end, EndState::Failed);
    assert_eq!(failure.unwrap().reason, FailureReason::LoopBoundReached);
    assert_eq!(state.passes.get("work"), Some(&2));
}

fn capped(cap: Option<f64>) -> Limits {
    Limits {
        run_max_cost_usd: cap,
        ..Limits::default()
    }
}

/// A fan run with `a`, `b` and `c` all in flight, under `cap`.
fn fan_in_flight(temp: &TempDir, cap: Option<f64>, runner: FakeRunner) -> Runtime<FakeRunner> {
    let mut runtime = Runtime::new(capped(cap), runner);
    runtime.add(spec(temp.path(), 1, FAN)).unwrap();
    runtime.tick();
    runtime.runner_mut().release_all();
    runtime.tick();
    assert_eq!(runtime.runner().in_flight().len(), 3);
    runtime
}

fn cap_events(runtime: &Runtime<FakeRunner>) -> Vec<Event> {
    runtime
        .events(&run_id(1))
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::CapReached { .. } | Event::Halted))
        .cloned()
        .collect()
}

#[test]
fn a_run_crossing_the_cost_cap_halts_naming_the_node_and_stops_its_sibling() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.cost("split", 0.25).cost("a", 1.5);
    let mut runtime = fan_in_flight(&temp, Some(1.0), runner);
    // The branches started with what the cap leaves after the split.
    assert_eq!(runtime.runner().budgets[1..], [Some(0.75); 3]);

    runtime.runner_mut().release_node(&run_id(1), "a");
    runtime.tick();
    let state = runtime.state(&run_id(1)).unwrap();
    assert_eq!(lifecycle(&runtime, 1), Lifecycle::Halted);
    let reached = state.cap_reached.clone().unwrap();
    assert_eq!(reached.key.node, "a");
    assert_eq!(reached.reason(), "cost cap of $1.00 reached at node a");
    assert_eq!(reached.cost_usd, 1.75);
    // The siblings were stopped, and stay recorded as running for a resume.
    assert!(runtime.runner().in_flight().is_empty());
    assert_eq!(runtime.runner().stopped.len(), 2);
    assert_eq!(state.running.len(), 2);
    assert!(runtime.in_flight(&run_id(1)).is_empty());
    assert!(!runtime.tick());
}

#[test]
fn resume_is_refused_over_the_cap_until_it_is_raised_then_the_run_finishes() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.cost("a", 1.5);
    let mut runtime = fan_in_flight(&temp, Some(1.0), runner);
    runtime.runner_mut().release_node(&run_id(1), "a");
    runtime.tick();

    let refused = runtime.resume(&run_id(1)).unwrap_err();
    assert_eq!(
        refused,
        RuntimeError::OverCap {
            run: run_id(1),
            cost: 1.5
        }
    );
    assert_eq!(
        refused.to_string(),
        format!(
            "run {} is over its cost cap: raise workflows.run_max_cost_usd above $1.50 first",
            run_id(1)
        )
    );
    assert_eq!(lifecycle(&runtime, 1), Lifecycle::Halted);

    // Raising it to exactly the cost is still over it.
    runtime.set_limits(capped(Some(1.5)));
    assert!(runtime.resume(&run_id(1)).is_err());

    runtime.set_limits(capped(Some(5.0)));
    let keys = runtime.resume(&run_id(1)).unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(runtime.state(&run_id(1)).unwrap().cap_reached, None);
    runtime.runner_mut().auto_finish = true;
    for _ in 0..10 {
        runtime.tick();
    }
    assert_eq!(
        lifecycle(&runtime, 1),
        Lifecycle::Ended(EndState::Succeeded)
    );
}

#[test]
fn no_cap_means_no_limit() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.auto_finish = true;
    runner.cost("work", 1_000_000.0);
    let mut runtime = runtime(runner);
    runtime.add(spec(temp.path(), 1, SINGLE)).unwrap();
    for _ in 0..5 {
        runtime.tick();
    }
    assert_eq!(
        lifecycle(&runtime, 1),
        Lifecycle::Ended(EndState::Succeeded)
    );
    assert_eq!(runtime.runner().budgets, [None]);
    assert_eq!(runtime.state(&run_id(1)).unwrap().cap_reached, None);
}

#[test]
fn a_run_with_no_finished_try_is_never_halted_by_the_cap() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(capped(Some(0.0)), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, SINGLE)).unwrap();
    runtime.tick();
    assert_eq!(lifecycle(&runtime, 1), Lifecycle::Running);
    assert!(cap_events(&runtime).is_empty());
}

#[test]
fn two_tries_finishing_in_one_tick_halt_the_run_once() {
    let temp = TempDir::new().unwrap();
    let mut runner = FakeRunner::new();
    runner.cost("a", 0.75).cost("b", 0.75);
    let mut runtime = fan_in_flight(&temp, Some(1.0), runner);
    runtime.runner_mut().release_node(&run_id(1), "a");
    runtime.runner_mut().release_node(&run_id(1), "b");
    runtime.tick();
    let events = cap_events(&runtime);
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], Event::CapReached { key, cost_usd, .. }
        if key.node == "b" && *cost_usd == 1.5));
    assert_eq!(events[1], Event::Halted);
    runtime.tick();
    assert_eq!(cap_events(&runtime).len(), 2);
}

#[test]
fn replay_treats_cap_reached_without_halted_as_halted_and_resumed_clears_it() {
    let reached = Event::CapReached {
        cap_usd: 5.0,
        cost_usd: 5.12,
        key: key("implement"),
    };
    let mut events = vec![Event::Created, reached];
    let state = replay(&events);
    assert_eq!(state.lifecycle(), Lifecycle::Halted);
    assert_eq!(
        state.cap_reached,
        Some(CapReached {
            cap_usd: 5.0,
            cost_usd: 5.12,
            key: key("implement"),
        })
    );
    assert_eq!(
        state.cap_reached.unwrap().reason(),
        "cost cap of $5.00 reached at node implement"
    );
    events.push(Event::Resumed);
    let state = replay(&events);
    assert!(!state.halted);
    assert_eq!(state.cap_reached, None);
}
