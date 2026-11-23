//! The handover of runs between two processes, each with its own runtime over the same logs in a
//! temporary directory (the runner is the scripted [`FakeRunner`] and starts no process): the
//! window drains, hands the runs over and unloads them; the next process loads them from disk.

use std::fs;
use std::path::Path;

use bongos_workflow_model::{Event, Limits, NodeKey, parse};
use tempfile::TempDir;

use crate::runtime::fake::FakeRunner;
use crate::runtime::{EVENTS_FILE, RunSpec, Runtime};

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

const WINDOW_PID: u32 = 100;
const HOST_PID: u32 = 200;

fn run_id(n: u32) -> String {
    format!("01890a5d-ac96-774b-bcce-b303{n:08x}")
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

fn started_nodes(runtime: &Runtime<FakeRunner>, n: u32) -> Vec<String> {
    runtime
        .runner()
        .started
        .iter()
        .filter(|(run, _)| *run == run_id(n))
        .map(|(_, key)| key.node.clone())
        .collect()
}

fn log_bytes(root: &Path, n: u32, text: &str) -> Vec<u8> {
    fs::read(spec(root, n, text).attempt_dir.join(EVENTS_FILE)).unwrap()
}

fn events_after_hand_over(runtime: &Runtime<FakeRunner>, n: u32) -> Vec<Event> {
    let events = runtime.events(&run_id(n)).unwrap();
    let at = events
        .iter()
        .position(|event| *event == Event::HandedOver { to_pid: HOST_PID })
        .expect("the run was handed over");
    events[at + 1..].to_vec()
}

/// The window's side of a handover of `ids`: drain (the caller finishes the nodes), then
/// `HandedOver` and unload.
fn hand_over(window: &mut Runtime<FakeRunner>, ids: &[String]) {
    window.hand_over_runs(HOST_PID, ids);
    for id in ids {
        assert!(window.unload(id));
    }
}

#[test]
fn a_run_in_a_long_node_finishes_it_in_the_window_and_continues_in_the_host() {
    let temp = TempDir::new().unwrap();
    let mut window = Runtime::new(Limits::default(), FakeRunner::new());
    window.add(spec(temp.path(), 1, TWO)).unwrap();
    window.tick();
    assert_eq!(window.in_flight(&run_id(1)), vec![key("plan")]);

    // The drain waits on the node and starts nothing new.
    let mut drained = window.drain();
    window.tick();
    assert_eq!(drained.try_recv(), Ok(None));
    window.runner_mut().release(&run_id(1));
    window.tick();
    assert_eq!(drained.try_recv(), Ok(Some(())));
    assert_eq!(started_nodes(&window, 1), vec!["plan"]);

    hand_over(&mut window, &[run_id(1)]);
    let handed = log_bytes(temp.path(), 1, TWO);

    // The window quits afterwards: it no longer holds the run, so the log is untouched.
    window.quit();
    window.tick();
    assert_eq!(log_bytes(temp.path(), 1, TWO), handed);

    // The host loads it from its checkpoint: `plan` is done, so only `work` starts.
    let mut host = Runtime::new(Limits::default(), FakeRunner::new());
    host.load(spec(temp.path(), 1, TWO)).unwrap();
    let state = host.state(&run_id(1)).unwrap();
    assert!(state.interrupted.is_empty() && state.running.is_empty());
    host.tick();
    assert_eq!(started_nodes(&host, 1), vec!["work"]);
    assert!(
        events_after_hand_over(&host, 1)
            .iter()
            .all(|event| !matches!(event, Event::NodeStarted { key, .. } if key.node == "plan")),
        "no node runs twice"
    );
    host.runner_mut().release(&run_id(1));
    host.tick();
    assert!(host.state(&run_id(1)).unwrap().is_over());
}

#[test]
fn interrupt_them_now_hands_over_a_run_that_loads_interrupted() {
    let temp = TempDir::new().unwrap();
    let mut window = Runtime::new(Limits::default(), FakeRunner::new());
    window.add(spec(temp.path(), 1, TWO)).unwrap();
    window.add(spec(temp.path(), 2, TWO)).unwrap();
    window.tick();
    let mut drained = window.drain();

    // Only the run handed over is interrupted; the other one keeps its node.
    assert!(window.interrupt_runs(&[run_id(1)]));
    assert_eq!(window.runner().stopped, vec![(run_id(1), key("plan"))]);
    assert_eq!(window.in_flight(&run_id(2)), vec![key("plan")]);
    assert_eq!(drained.try_recv(), Ok(None));
    window.runner_mut().release(&run_id(2));
    window.tick();
    assert_eq!(drained.try_recv(), Ok(Some(())));

    hand_over(&mut window, &[run_id(1)]);
    let mut host = Runtime::new(Limits::default(), FakeRunner::new());
    host.load(spec(temp.path(), 1, TWO)).unwrap();
    let state = host.state(&run_id(1)).unwrap();
    assert!(state.interrupted.contains_key(&key("plan")), "{state:?}");
    host.tick();
    assert!(
        started_nodes(&host, 1).is_empty(),
        "waits on the person, not continued"
    );
}

#[test]
fn a_run_at_a_gate_is_handed_over_as_it_is_and_never_interrupted() {
    let temp = TempDir::new().unwrap();
    let mut window = Runtime::new(Limits::default(), FakeRunner::new());
    window.add(spec(temp.path(), 1, GATED)).unwrap();
    window.tick();
    assert!(
        window
            .state(&run_id(1))
            .unwrap()
            .open_gates
            .contains_key(&key("work"))
    );

    // No process: the drain resolves at once and nothing is interrupted.
    let mut drained = window.drain();
    assert_eq!(drained.try_recv(), Ok(Some(())));
    hand_over(&mut window, &[run_id(1)]);
    window.quit();

    let mut host = Runtime::new(Limits::default(), FakeRunner::new());
    host.load(spec(temp.path(), 1, GATED)).unwrap();
    let events = host.events(&run_id(1)).unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Interrupted { .. }))
    );
    let state = host.state(&run_id(1)).unwrap();
    assert!(state.interrupted.is_empty());
    assert_eq!(state.open_gates.get(&key("work")).unwrap(), "Go ahead?");
    host.tick();
    assert!(started_nodes(&host, 1).is_empty(), "the gate still waits");
}

#[test]
fn hand_over_runs_appends_only_to_the_named_live_runs() {
    let temp = TempDir::new().unwrap();
    let mut window = Runtime::new(Limits::default(), FakeRunner::new());
    window.add(spec(temp.path(), 1, TWO)).unwrap();
    window.add(spec(temp.path(), 2, TWO)).unwrap();
    window.tick();
    let other = log_bytes(temp.path(), 2, TWO);
    window.hand_over_runs(WINDOW_PID, &[run_id(1)]);
    window.hand_over_runs(WINDOW_PID, &[run_id(1)]);
    let handed = window
        .events(&run_id(1))
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::HandedOver { .. }))
        .count();
    assert_eq!(handed, 2, "each call records who holds the run next");
    assert_eq!(log_bytes(temp.path(), 2, TWO), other);
}
