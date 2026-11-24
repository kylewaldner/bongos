//! Tests of built-in start gates: a built-in that asks a question before a try starts holds the
//! key on a `person_only` gate until a person approves exactly that question, every new try asks
//! again, a cancel fails the key for good, and the gate survives a reload. Logs live in temporary
//! directories; the built-in is a fake that records its starts and finishes when told.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bongos_workflow_model::record::{EndState, parse_log, start_gate_approved};
use bongos_workflow_model::{Event, Failure, FailureReason, Limits, NodeKey, parse, replay};
use serde_json::json;
use tempfile::TempDir;

use crate::builtin_nodes::{BuiltinNode, BuiltinNodes, BuiltinStart};
use crate::lifecycle::BY_PERSON;
use crate::runtime::fake::FakeRunner;
use crate::runtime::{EVENTS_FILE, Finish, RunSpec, Runtime, RuntimeError};

const POSTED: &str = r#"
schema = 1
name = "posted"
start = "post"

[nodes.post]
kind = "program"
use = "post"
output = { link = { type = "text" } }
routes = [{ to = "done" }]

[ends.done]
kind = "success"
"#;

const QUESTION: &str = "Post this to #team?\n\nThe build is green.";

/// A built-in that asks `question` (when set) before each try and finishes only when told.
#[derive(Default)]
struct FakePost {
    question: Mutex<Option<String>>,
    started: Mutex<Vec<(String, NodeKey, u32)>>,
    finishes: Mutex<Vec<Finish>>,
}

impl FakePost {
    fn asking(question: &str) -> Arc<Self> {
        let node = Self::default();
        *node.question.lock().unwrap() = Some(question.to_string());
        Arc::new(node)
    }

    fn ask(&self, question: &str) {
        *self.question.lock().unwrap() = Some(question.to_string());
    }

    fn starts(&self) -> usize {
        self.started.lock().unwrap().len()
    }

    /// Finishes the last started try with `outcome`.
    fn finish(&self, outcome: Result<serde_json::Value, Failure>) {
        let (run, key, try_number) = self.started.lock().unwrap().last().cloned().unwrap();
        let mut finish = Finish::ok(run, key, try_number, json!(null));
        finish.outcome = outcome;
        self.finishes.lock().unwrap().push(finish);
    }
}

impl BuiltinNode for FakePost {
    fn start_gate(&self, _start: &BuiltinStart<'_>) -> Option<String> {
        self.question.lock().unwrap().clone()
    }

    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure> {
        self.started.lock().unwrap().push((
            start.run.to_string(),
            start.key.clone(),
            start.try_number,
        ));
        Ok(())
    }

    fn poll(&self) -> Vec<Finish> {
        std::mem::take(&mut *self.finishes.lock().unwrap())
    }

    fn stop(&self, _tries: &[(String, NodeKey)]) {}
}

fn run_id() -> String {
    "01890a5d-ac96-774b-bcce-b30200000001".to_string()
}

fn spec(root: &Path) -> RunSpec {
    RunSpec {
        id: run_id(),
        definition: parse(POSTED).unwrap(),
        attempt_dir: root.join(format!("runs/{}/attempts/1", run_id())),
        loop_bound: 5,
    }
}

fn key() -> NodeKey {
    NodeKey::new("post", 1)
}

fn runtime(node: &Arc<FakePost>) -> Runtime<FakeRunner> {
    let builtins = BuiltinNodes::default();
    builtins.insert("post", node.clone());
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    runtime.set_builtins(builtins);
    runtime
}

fn gates_opened(runtime: &Runtime<FakeRunner>) -> Vec<Event> {
    runtime
        .events(&run_id())
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::GateOpened { .. }))
        .cloned()
        .collect()
}

fn opened(question: &str) -> Event {
    Event::GateOpened {
        key: key(),
        question: question.to_string(),
        person_only: true,
    }
}

fn approve(runtime: &mut Runtime<FakeRunner>) -> Result<(), RuntimeError> {
    runtime.close_gate(&run_id(), key(), BY_PERSON, None, None)
}

fn log_events(root: &Path) -> Vec<Event> {
    let bytes = fs::read(spec(root).attempt_dir.join(EVENTS_FILE)).unwrap();
    parse_log(bytes).unwrap().events
}

#[test]
fn a_start_gate_builtin_with_a_question_opens_one_gate_and_does_not_start() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();
    runtime.tick();

    assert_eq!(node.starts(), 0);
    assert_eq!(gates_opened(&runtime), [opened(QUESTION)]);
    let state = runtime.state(&run_id()).unwrap();
    assert!(state.person_only_gates.contains(&key()));
    assert!(state.running.is_empty());
    assert!(runtime.in_flight(&run_id()).is_empty());
    // The gate is in the log with its flag, so a reload reads it as a start gate.
    assert!(log_events(temp.path()).contains(&opened(QUESTION)));
}

#[test]
fn a_start_gate_builtin_without_a_question_starts_at_once() {
    let temp = TempDir::new().unwrap();
    let node = Arc::new(FakePost::default());
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();

    assert_eq!(node.starts(), 1);
    assert!(gates_opened(&runtime).is_empty());
}

#[test]
fn a_start_gate_approved_starts_the_key_once_and_a_second_approval_is_refused() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();

    approve(&mut runtime).unwrap();
    runtime.tick();
    runtime.tick();
    assert_eq!(node.starts(), 1);
    assert_eq!(gates_opened(&runtime).len(), 1);
    assert_eq!(
        approve(&mut runtime),
        Err(RuntimeError::NoGate(key())),
        "the gate is spent"
    );

    node.finish(Ok(json!({ "link": "https://example.invalid/p1" })));
    runtime.tick();
    let state = runtime.state(&run_id()).unwrap();
    assert_eq!(state.ended, Some((EndState::Succeeded, None)));
    assert_eq!(node.starts(), 1);
}

#[test]
fn a_start_gate_with_a_changed_question_after_approval_asks_again() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();
    approve(&mut runtime).unwrap();

    // The text changed between the approval and the start: the approval was for other text.
    node.ask("Post this to #team?\n\nThe build is red.");
    runtime.tick();
    assert_eq!(node.starts(), 0);
    assert_eq!(
        gates_opened(&runtime),
        [
            opened(QUESTION),
            opened("Post this to #team?\n\nThe build is red.")
        ]
    );
}

#[test]
fn a_start_gate_asks_again_for_every_new_try() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();
    approve(&mut runtime).unwrap();
    runtime.tick();
    assert_eq!(node.starts(), 1);

    // A retried failure: the next try asks again with the same question.
    node.finish(Err(Failure::new(
        FailureReason::ServiceUnavailable,
        Some(key()),
        "slack is down",
    )));
    runtime.tick();
    assert_eq!(node.starts(), 1);
    assert_eq!(gates_opened(&runtime).len(), 2);
    approve(&mut runtime).unwrap();
    runtime.tick();
    assert_eq!(node.starts(), 2);
}

#[test]
fn a_start_gate_cancelled_fails_the_key_with_gate_cancelled_and_is_not_retried() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();

    runtime.cancel_gate(&run_id(), key()).unwrap();
    runtime.tick();
    runtime.tick();
    assert_eq!(node.starts(), 0);
    assert_eq!(gates_opened(&runtime).len(), 1, "no new gate: not retried");
    let state = runtime.state(&run_id()).unwrap();
    assert!(state.open_gates.is_empty());
    assert_eq!(
        state
            .last_finish(&key())
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .reason,
        FailureReason::GateCancelled
    );
    let (end, failure) = state.ended.unwrap();
    assert_eq!(end, EndState::Failed);
    assert_eq!(failure.unwrap().reason, FailureReason::GateCancelled);
    // Cancelling again finds no start gate.
    assert_eq!(
        runtime.cancel_gate(&run_id(), key()),
        Err(RuntimeError::NoGate(key()))
    );
}

#[test]
fn a_start_gate_refuses_an_edit_or_a_revision_and_stays_open() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();

    let edit = runtime.close_gate(
        &run_id(),
        key(),
        BY_PERSON,
        Some(json!({ "text": "other" })),
        None,
    );
    let revise = runtime.close_gate(
        &run_id(),
        key(),
        BY_PERSON,
        None,
        Some("say it nicer".to_string()),
    );
    assert!(matches!(edit, Err(RuntimeError::StartGate(_))), "{edit:?}");
    assert!(
        matches!(revise, Err(RuntimeError::StartGate(_))),
        "{revise:?}"
    );
    runtime.tick();
    assert_eq!(node.starts(), 0);
    assert!(
        runtime
            .state(&run_id())
            .unwrap()
            .open_gates
            .contains_key(&key())
    );
}

#[test]
fn a_start_gate_open_at_a_reload_stays_open_and_the_key_does_not_start() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut crashed = runtime(&node);
    crashed.add(spec(temp.path())).unwrap();
    crashed.tick();
    drop(crashed);

    let mut loaded = runtime(&node);
    loaded.load(spec(temp.path())).unwrap();
    loaded.tick();
    loaded.tick();
    assert_eq!(node.starts(), 0);
    assert_eq!(gates_opened(&loaded), [opened(QUESTION)]);
    let state = loaded.state(&run_id()).unwrap();
    assert_eq!(
        state.open_gates.get(&key()).map(String::as_str),
        Some(QUESTION)
    );
    assert!(state.person_only_gates.contains(&key()));

    approve(&mut loaded).unwrap();
    loaded.tick();
    assert_eq!(node.starts(), 1);
}

#[test]
fn a_start_gate_closed_before_a_crash_starts_the_key_once_after_the_reload() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut crashed = runtime(&node);
    crashed.add(spec(temp.path())).unwrap();
    crashed.tick();
    // The crash comes after the approval's `GateClosed` and before the start.
    approve(&mut crashed).unwrap();
    drop(crashed);

    let mut loaded = runtime(&node);
    loaded.load(spec(temp.path())).unwrap();
    loaded.tick();
    loaded.tick();
    assert_eq!(node.starts(), 1);
    assert_eq!(gates_opened(&loaded).len(), 1);
}

#[test]
fn a_start_gate_spent_by_a_start_before_a_crash_asks_again_for_the_next_try() {
    let events = vec![
        Event::Created,
        opened(QUESTION),
        Event::GateClosed {
            key: key(),
            by: BY_PERSON.to_string(),
            edit: None,
            comment: None,
        },
    ];
    assert!(start_gate_approved(&events, &key(), QUESTION, BY_PERSON));
    assert!(!start_gate_approved(&events, &key(), "other", BY_PERSON));
    assert!(!start_gate_approved(&events, &key(), QUESTION, "lane"));
    let mut spent = events.clone();
    spent.push(Event::NodeStarted {
        key: key(),
        try_number: 1,
        pid: 0,
        pid_start_time: 0,
    });
    assert!(!start_gate_approved(&spent, &key(), QUESTION, BY_PERSON));
    // A static gate closed by a person is no approval of a start gate.
    let static_gate = vec![
        Event::GateOpened {
            key: key(),
            question: QUESTION.to_string(),
            person_only: false,
        },
        events[2].clone(),
    ];
    assert!(!start_gate_approved(
        &static_gate,
        &key(),
        QUESTION,
        BY_PERSON
    ));
    assert!(replay(&spent).open_gates.is_empty());
}

#[test]
fn a_start_gate_old_log_without_the_person_only_field_replays_as_a_static_gate() {
    let line = r#"{"event":"GateOpened","key":{"node":"post","pass":1},"question":"Go?"}"#;
    let event: Event = serde_json::from_str(line).unwrap();
    assert_eq!(
        event,
        Event::GateOpened {
            key: key(),
            question: "Go?".to_string(),
            person_only: false,
        }
    );
    let state = replay(&[Event::Created, event.clone()]);
    assert_eq!(
        state.open_gates.get(&key()).map(String::as_str),
        Some("Go?")
    );
    assert!(state.person_only_gates.is_empty());
    // A gate that is not a start gate is written as before, without the field.
    assert!(!event.to_line().contains("person_only"));
    assert!(opened("Go?").to_line().contains("\"person_only\":true"));
}

#[test]
fn a_start_gate_refuses_a_cli_peers_approve_or_revise_and_passes_a_windows_approve() {
    use crate::lifecycle::{PERSON_ONLY_REFUSAL, refuse_unattested};

    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();

    // `Approve` and `Revise` from the command line (`remote::execute` checks both before they
    // close the gate) are refused before anything is written.
    let state = runtime.state(&run_id()).unwrap();
    let refusal = refuse_unattested(&state, &key(), false).unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "approving a Slack post needs a person at a bongos window"
    );
    assert_eq!(refusal.message, PERSON_ONLY_REFUSAL);
    runtime.tick();
    assert_eq!(node.starts(), 0);
    assert!(
        runtime
            .state(&run_id())
            .unwrap()
            .open_gates
            .contains_key(&key())
    );

    // A window speaks for a person: its approve passes and the key starts.
    let state = runtime.state(&run_id()).unwrap();
    refuse_unattested(&state, &key(), true).unwrap();
    approve(&mut runtime).unwrap();
    runtime.tick();
    assert_eq!(node.starts(), 1);

    // A gate that is not a start gate is the verb's business, from any peer.
    let static_gate = replay(&[
        Event::Created,
        Event::GateOpened {
            key: key(),
            question: QUESTION.to_string(),
            person_only: false,
        },
    ]);
    refuse_unattested(&static_gate, &key(), false).unwrap();
}

#[test]
fn a_start_gate_post_not_sent_fails_the_key_without_asking_again() {
    let temp = TempDir::new().unwrap();
    let node = FakePost::asking(QUESTION);
    let mut runtime = runtime(&node);
    runtime.add(spec(temp.path())).unwrap();
    runtime.tick();
    approve(&mut runtime).unwrap();
    runtime.tick();
    node.finish(Err(Failure::new(
        FailureReason::PostNotSent,
        Some(key()),
        "the post may not have been sent",
    )));
    runtime.tick();
    runtime.tick();

    assert_eq!(node.starts(), 1);
    assert_eq!(
        gates_opened(&runtime).len(),
        1,
        "no new try, so no new gate"
    );
    let (end, failure) = runtime.state(&run_id()).unwrap().ended.unwrap();
    assert_eq!(end, EndState::Failed);
    assert_eq!(failure.unwrap().reason, FailureReason::PostNotSent);
}
