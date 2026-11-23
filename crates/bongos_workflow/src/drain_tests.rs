//! Tests of who counts toward the run limit (lane and landing runs do not), of the drain that
//! starts nothing new and resolves once no node runs, of the hand-over event and how a relaunch
//! reads it, and of unloading a run without writing. Logs live in temporary directories; the
//! runner is the scripted [`FakeRunner`] and starts no process.

use std::fs;
use std::path::Path;

use bongos_workflow_model::record::{StartedBy, parse_log};
use bongos_workflow_model::{Event, Limits, NodeKey, parse, replay};
use tempfile::TempDir;

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

const TWO: &str = r#"
schema = 1
name = "two"
start = "plan"

[nodes.plan]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "work" }]

[nodes.work]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const CHECKED: &str = r#"
schema = 1
name = "checked"
start = "check"

[nodes.check]
kind = "program"
use = "checks"
routes = [
  { when = { verdict = "pass" }, to = "done" },
  { when = { verdict = "fail" }, to = "done" },
]

[ends.done]
kind = "success"
"#;

fn run_id(n: u32) -> String {
    format!("01890a5d-ac96-774b-bcce-b302{n:08x}")
}

fn spec(root: &Path, n: u32, text: &str) -> RunSpec {
    RunSpec {
        id: run_id(n),
        definition: parse(text).unwrap(),
        attempt_dir: root.join(format!("runs/{}/attempts/1", run_id(n))),
        loop_bound: 5,
    }
}

fn key(node: &str) -> NodeKey {
    NodeKey::new(node, 1)
}

fn started_runs(runtime: &Runtime<FakeRunner>) -> Vec<String> {
    runtime
        .runner()
        .started
        .iter()
        .map(|(run, _)| run.clone())
        .collect()
}

fn started_nodes(runtime: &Runtime<FakeRunner>, n: u32) -> Vec<String> {
    runtime
        .runner()
        .started
        .iter()
        .filter(|(run, _)| *run == run_id(n))
        .map(|(_, key)| key.node.clone())
        .collect()
}

fn add(runtime: &mut Runtime<FakeRunner>, root: &Path, n: u32, text: &str, by: StartedBy) {
    runtime.add(spec(root, n, text)).unwrap();
    runtime.set_started_by(&run_id(n), by);
}

/// Writes `events` as the log of run `n`.
fn write_log(root: &Path, n: u32, events: &[Event]) {
    let dir = spec(root, n, TWO).attempt_dir;
    fs::create_dir_all(&dir).unwrap();
    let text: String = events.iter().map(Event::to_line).collect();
    fs::write(dir.join(EVENTS_FILE), text).unwrap();
}

fn started(node: &str, try_number: u32) -> Event {
    Event::NodeStarted {
        key: key(node),
        try_number,
        pid: 0,
        pid_start_time: 0,
    }
}

fn finished(node: &str) -> Event {
    Event::NodeFinished {
        key: key(node),
        try_number: 1,
        result: Some(serde_json::json!({ "summary": "ok" })),
        failure: None,
        cost_usd: 0.0,
        turns: None,
        elapsed_ms: 0,
        session_id: None,
    }
}

#[test]
fn three_hand_started_and_two_lane_runs_all_run_and_a_fourth_hand_start_queues() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    for n in 1..=4 {
        add(&mut runtime, temp.path(), n, SINGLE, StartedBy::Person);
    }
    add(&mut runtime, temp.path(), 5, SINGLE, StartedBy::Lane);
    add(&mut runtime, temp.path(), 6, SINGLE, StartedBy::Lane);
    runtime.tick();

    assert_eq!(
        started_runs(&runtime),
        vec![run_id(1), run_id(2), run_id(3), run_id(5), run_id(6)]
    );
    assert!(runtime.is_queued(&run_id(4)));
    assert_eq!(
        runtime.events(&run_id(4)).unwrap(),
        &[Event::Created, Event::Queued]
    );

    // A lane run ending frees no hand-started slot.
    runtime.runner_mut().release(&run_id(5));
    runtime.tick();
    assert!(runtime.is_queued(&run_id(4)));

    // A hand-started run ending does.
    runtime.runner_mut().release(&run_id(1));
    runtime.tick();
    assert!(!runtime.is_queued(&run_id(4)));
    assert_eq!(started_nodes(&runtime, 4), vec!["work"]);
}

#[test]
fn a_landing_run_starts_past_the_run_limit_and_counts_toward_sessions() {
    let temp = TempDir::new().unwrap();
    let limits = Limits {
        max_concurrent_runs: 3,
        max_concurrent_sessions: Some(4),
        ..Limits::default()
    };
    let mut runtime = Runtime::new(limits, FakeRunner::new());
    for n in 1..=3 {
        add(&mut runtime, temp.path(), n, SINGLE, StartedBy::Person);
    }
    add(&mut runtime, temp.path(), 4, SINGLE, StartedBy::Landing);
    add(&mut runtime, temp.path(), 5, SINGLE, StartedBy::Landing);
    runtime.tick();

    assert_eq!(
        started_runs(&runtime),
        vec![run_id(1), run_id(2), run_id(3), run_id(4)]
    );
    assert_eq!(runtime.sessions_in_flight(), 4);
    // The second landing run is past the run limit too, but every session is taken.
    assert!(started_nodes(&runtime, 5).is_empty());
    assert!(!runtime.is_queued(&run_id(5)));

    runtime.runner_mut().release(&run_id(4));
    runtime.tick();
    assert_eq!(started_nodes(&runtime, 5), vec!["work"]);
    assert_eq!(runtime.sessions_in_flight(), 4);
}

#[test]
fn drain_resolves_only_after_running_nodes_finish_and_starts_none() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, TWO)).unwrap();
    runtime.add(spec(temp.path(), 2, TWO)).unwrap();
    runtime.tick();
    assert_eq!(runtime.nodes_in_flight(), 2);

    let mut drained = runtime.drain();
    runtime.tick();
    assert_eq!(drained.try_recv(), Ok(None));

    runtime.runner_mut().release(&run_id(1));
    runtime.tick();
    assert_eq!(drained.try_recv(), Ok(None));
    assert_eq!(started_nodes(&runtime, 1), vec!["plan"]);
    assert!(
        runtime
            .events(&run_id(1))
            .unwrap()
            .contains(&finished("plan"))
    );

    runtime.runner_mut().release(&run_id(2));
    runtime.tick();
    assert_eq!(drained.try_recv(), Ok(Some(())));
    assert_eq!(runtime.nodes_in_flight(), 0);
    assert_eq!(started_nodes(&runtime, 2), vec!["plan"]);

    // A rerun or a resume starts nothing while draining.
    assert_eq!(
        runtime.rerun(&run_id(1), key("work")),
        Err(RuntimeError::Draining)
    );
    runtime.tick();
    assert_eq!(runtime.runner().started.len(), 2);
}

#[test]
fn undrain_resumes_starting() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, TWO)).unwrap();
    runtime.tick();

    let mut cancelled = runtime.drain();
    runtime.undrain();
    assert!(cancelled.try_recv().is_err(), "an undrain drops the drain");

    let _drained = runtime.drain();
    runtime.runner_mut().release_all();
    runtime.tick();
    runtime.tick();
    assert_eq!(started_nodes(&runtime, 1), vec!["plan"]);

    runtime.undrain();
    runtime.tick();
    assert_eq!(started_nodes(&runtime, 1), vec!["plan", "work"]);
    assert!(!runtime.is_draining());
}

#[test]
fn a_drained_check_counts_as_parked_and_starts_again_after_a_hand_over() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, CHECKED)).unwrap();
    runtime.tick();
    assert_eq!(runtime.in_flight(&run_id(1)), vec![key("check")]);

    let mut drained = runtime.drain();
    runtime.tick();
    assert_eq!(drained.try_recv(), Ok(None));
    runtime.checks_drained(&run_id(1), &key("check")).unwrap();
    assert_eq!(drained.try_recv(), Ok(Some(())));
    assert_eq!(runtime.runner().stopped, vec![(run_id(1), key("check"))]);

    runtime.hand_over(4242);
    let events = runtime.events(&run_id(1)).unwrap().to_vec();
    assert_eq!(
        &events[events.len() - 2..],
        &[
            Event::ChecksDrained { key: key("check") },
            Event::HandedOver { to_pid: 4242 }
        ]
    );

    // The process it was handed to loads it live and starts the check again.
    let mut next = Runtime::new(Limits::default(), FakeRunner::new());
    next.load(spec(temp.path(), 1, CHECKED)).unwrap();
    let state = next.state(&run_id(1)).unwrap();
    assert!(state.interrupted.is_empty());
    assert!(state.running.is_empty());
    next.tick();
    assert_eq!(next.in_flight(&run_id(1)), vec![key("check")]);
    let events = next.events(&run_id(1)).unwrap();
    assert_eq!(events[events.len() - 2], Event::HandedOver { to_pid: 4242 });
    assert!(matches!(
        events.last(),
        Some(Event::NodeStarted { key: again, try_number: 2, .. }) if *again == key("check")
    ));
}

#[test]
fn a_handed_over_run_replays_as_live() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, TWO)).unwrap();
    runtime.tick();
    let mut drained = runtime.drain();
    runtime.runner_mut().release_all();
    runtime.tick();
    assert_eq!(drained.try_recv(), Ok(Some(())));
    runtime.hand_over(7);
    let before = runtime.events(&run_id(1)).unwrap().to_vec();
    assert_eq!(before.last(), Some(&Event::HandedOver { to_pid: 7 }));
    let state = replay(&before);
    assert!(state.running.is_empty() && state.interrupted.is_empty());

    let mut next = Runtime::new(Limits::default(), FakeRunner::new());
    next.load(spec(temp.path(), 1, TWO)).unwrap();
    assert_eq!(next.events(&run_id(1)).unwrap(), before.as_slice());
    assert!(next.is_queued(&run_id(1)));
    next.tick();
    assert_eq!(started_nodes(&next, 1), vec!["work"]);
}

#[test]
fn a_hand_over_after_an_unfinished_or_interrupted_node_replays_interrupted() {
    let interrupted = replay(&[
        Event::Created,
        started("plan", 1),
        Event::Interrupted {
            keys: vec![key("plan")],
        },
        Event::HandedOver { to_pid: 9 },
    ]);
    assert!(interrupted.interrupted.contains_key(&key("plan")));

    // A started key with no finish and no `ChecksDrained` is still running in the log, so the
    // load marks it interrupted as it would without the hand-over.
    let temp = TempDir::new().unwrap();
    write_log(
        temp.path(),
        1,
        &[
            Event::Created,
            started("plan", 1),
            Event::HandedOver { to_pid: 9 },
        ],
    );
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.load(spec(temp.path(), 1, TWO)).unwrap();
    let state = runtime.state(&run_id(1)).unwrap();
    assert!(state.interrupted.contains_key(&key("plan")));
    assert_eq!(
        runtime.events(&run_id(1)).unwrap().last(),
        Some(&Event::Interrupted {
            keys: vec![key("plan")]
        })
    );
    runtime.tick();
    assert!(runtime.runner().started.is_empty());
}

#[test]
fn a_crash_after_some_hand_over_appends_loses_no_run() {
    let temp = TempDir::new().unwrap();
    let checkpoint = [Event::Created, started("plan", 1), finished("plan")];
    let mut handed = checkpoint.to_vec();
    handed.push(Event::HandedOver { to_pid: 11 });
    write_log(temp.path(), 1, &handed);
    write_log(temp.path(), 2, &checkpoint);

    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    let loaded = runtime
        .load_all(vec![spec(temp.path(), 1, TWO), spec(temp.path(), 2, TWO)])
        .unwrap();
    assert!(loaded.iter().all(Result::is_ok));
    assert_eq!(runtime.run_ids(), vec![run_id(1), run_id(2)]);
    runtime.tick();
    assert_eq!(started_nodes(&runtime, 1), vec!["work"]);
    assert_eq!(started_nodes(&runtime, 2), vec!["work"]);
}

#[test]
fn unload_appends_no_event() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, TWO)).unwrap();
    runtime.tick();
    let path = spec(temp.path(), 1, TWO).attempt_dir.join(EVENTS_FILE);
    let before = fs::read(&path).unwrap();

    assert!(runtime.unload(&run_id(1)));
    assert!(!runtime.unload(&run_id(1)));
    assert!(runtime.run_ids().is_empty());
    assert!(runtime.runner().stopped.is_empty());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn the_new_events_are_tagged_like_the_others_and_old_logs_parse_unchanged() {
    let lines = [
        Event::HandedOver { to_pid: 3 }.to_line(),
        Event::ChecksDrained { key: key("check") }.to_line(),
    ];
    assert_eq!(lines[0], "{\"event\":\"HandedOver\",\"to_pid\":3}\n");
    assert!(lines[1].starts_with("{\"event\":\"ChecksDrained\",\"key\":"));
    let parsed = parse_log(lines.concat()).unwrap();
    assert_eq!(
        parsed.events,
        vec![
            Event::HandedOver { to_pid: 3 },
            Event::ChecksDrained { key: key("check") }
        ]
    );

    let old = "{\"event\":\"Created\"}\n{\"event\":\"Queued\"}\n";
    assert_eq!(
        parse_log(old).unwrap().events,
        vec![Event::Created, Event::Queued]
    );
}

// Two runs in a check; the drain stopped only run 1's (task checks named just that key): run 1
// gets `ChecksDrained` and comes back resumable where it is loaded next, while run 2's started
// check, with neither a finish nor `ChecksDrained`, comes back interrupted.
#[test]
fn only_the_drained_check_resumes_and_a_started_check_with_neither_replays_interrupted() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, CHECKED)).unwrap();
    runtime.add(spec(temp.path(), 2, CHECKED)).unwrap();
    runtime.tick();
    let _drained = runtime.drain();
    runtime.checks_drained(&run_id(1), &key("check")).unwrap();
    assert_eq!(runtime.in_flight(&run_id(2)), vec![key("check")]);
    runtime.hand_over(5);

    let drained_in = |n: u32| {
        runtime
            .events(&run_id(n))
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::ChecksDrained { .. }))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        drained_in(1),
        vec![Event::ChecksDrained { key: key("check") }]
    );
    assert!(drained_in(2).is_empty());

    let one = replay(runtime.events(&run_id(1)).unwrap());
    assert!(one.running.is_empty() && one.interrupted.is_empty());
    let two = replay(runtime.events(&run_id(2)).unwrap());
    assert!(two.running.contains_key(&key("check")));

    let mut next = Runtime::new(Limits::default(), FakeRunner::new());
    let loaded = next
        .load_all(vec![
            spec(temp.path(), 1, CHECKED),
            spec(temp.path(), 2, CHECKED),
        ])
        .unwrap();
    assert!(loaded.iter().all(Result::is_ok));
    assert!(next.state(&run_id(1)).unwrap().interrupted.is_empty());
    assert!(
        next.state(&run_id(2))
            .unwrap()
            .interrupted
            .contains_key(&key("check"))
    );
    next.tick();
    assert_eq!(started_nodes(&next, 1), vec!["check"]);
    assert!(started_nodes(&next, 2).is_empty());
}

// A check that runs for hours (the fake runner never ends it) does not hold the drain: it
// resolves as soon as the other run's node finished and the check is recorded drained.
#[test]
fn a_long_check_does_not_delay_the_drain() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.add(spec(temp.path(), 1, CHECKED)).unwrap();
    runtime.add(spec(temp.path(), 2, TWO)).unwrap();
    runtime.tick();
    assert_eq!(runtime.nodes_in_flight(), 2);

    let mut drained = runtime.drain();
    runtime.runner_mut().release(&run_id(2));
    runtime.tick();
    assert_eq!(drained.try_recv(), Ok(None), "the check still runs");
    runtime.checks_drained(&run_id(1), &key("check")).unwrap();
    assert_eq!(drained.try_recv(), Ok(Some(())));
    assert_eq!(runtime.nodes_in_flight(), 0);
}

// The process a run in a drained check is handed to loads it and starts the check again from its
// checkpoint by itself: nothing asks a person (no `Resumed`), as it would for an interruption
// (a lane's `on_interruption = "ask"`, or a loaded interrupted run, which gets no command).
#[test]
fn a_drained_check_resumes_after_load_all_with_no_person() {
    let temp = TempDir::new().unwrap();
    let dir = spec(temp.path(), 1, CHECKED).attempt_dir;
    fs::create_dir_all(&dir).unwrap();
    let log: String = [
        Event::Created,
        started("check", 1),
        Event::ChecksDrained { key: key("check") },
        Event::HandedOver { to_pid: 6 },
    ]
    .iter()
    .map(Event::to_line)
    .collect();
    fs::write(dir.join(EVENTS_FILE), log).unwrap();

    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    let loaded = runtime
        .load_all(vec![spec(temp.path(), 1, CHECKED)])
        .unwrap();
    assert!(loaded.iter().all(Result::is_ok));
    let state = runtime.state(&run_id(1)).unwrap();
    assert!(state.interrupted.is_empty() && state.running.is_empty());
    runtime.tick();
    assert_eq!(runtime.in_flight(&run_id(1)), vec![key("check")]);
    let events = runtime.events(&run_id(1)).unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Interrupted { .. } | Event::Resumed))
    );
    assert!(matches!(
        events.last(),
        Some(Event::NodeStarted { key: again, try_number: 2, .. }) if *again == key("check")
    ));
}
