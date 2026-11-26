//! The runtime with a scripted node admission: a waiting key appends nothing and holds no session
//! slot, a refused one fails like a start failure, and every exit of a try or a run tells the
//! admission once. Logs live in temporary directories; no process is spawned.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::record::EndState;
use bongos_workflow_model::{
    Event, FailureReason, Layer, Limits, NodeKey, Resolved, Severity, ValidationContext, parse,
};
use tempfile::TempDir;

use crate::admission::{Admission, NEEDS_NOT_AVAILABLE, NodeAdmission, NodeWait};
use crate::process::ProcessRecord;
use crate::runtime::fake::FakeRunner;
use crate::runtime::{RunSpec, Runtime};

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

/// Two branches: `a` and `b` run side by side and join at `gather`.
const PAIR: &str = r#"
schema = 1
name = "pair"
start = "split"

[nodes.split]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = ["a", "b"] }]

[nodes.a]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "gather" }]

[nodes.b]
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

/// What the scripted admission answers and what it was told.
#[derive(Default)]
struct Script {
    /// By node name; a node not named goes.
    answers: BTreeMap<String, Admission>,
    /// Every call, as one line ("admit work", "started work", "finished work", "waiting",
    /// "ended").
    calls: Vec<String>,
}

#[derive(Clone, Default)]
struct Scripted(Arc<Mutex<Script>>);

impl Scripted {
    fn answer(&self, node: &str, admission: Admission) {
        self.0
            .lock()
            .unwrap()
            .answers
            .insert(node.to_string(), admission);
    }

    fn wait(&self, node: &str) {
        self.answer(
            node,
            Admission::Wait(NodeWait {
                text: format!("waiting for {node}"),
            }),
        );
    }

    fn go(&self, node: &str) {
        self.answer(node, Admission::Go);
    }

    /// The calls since the last take, without the admits.
    fn told(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap().calls)
            .into_iter()
            .filter(|call| !call.starts_with("admit"))
            .collect()
    }

    fn admits(&self, node: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|call| **call == format!("admit {node}"))
            .count()
    }

    fn record(&self, call: String) {
        self.0.lock().unwrap().calls.push(call);
    }
}

impl NodeAdmission for Scripted {
    fn admit(
        &mut self,
        _run: &str,
        _definition: &bongos_workflow_model::Definition,
        key: &NodeKey,
    ) -> Admission {
        self.record(format!("admit {}", key.node));
        self.0
            .lock()
            .unwrap()
            .answers
            .get(&key.node)
            .cloned()
            .unwrap_or(Admission::Go)
    }

    fn started(&mut self, _run: &str, key: &NodeKey, _process: ProcessRecord) {
        self.record(format!("started {}", key.node));
    }

    fn finished(&mut self, _run: &str, key: &NodeKey) {
        self.record(format!("finished {}", key.node));
    }

    fn run_waiting(&mut self, _run: &str) {
        self.record("waiting".to_string());
    }

    fn run_ended(&mut self, _run: &str) {
        self.record("ended".to_string());
    }
}

const RUN: &str = "01890a5d-ac96-774b-bcce-b30200000001";

fn runtime(temp: &TempDir, text: &str, limits: Limits) -> (Runtime<FakeRunner>, Scripted) {
    let mut runtime = Runtime::new(limits, FakeRunner::new());
    let scripted = Scripted::default();
    runtime.set_admission(Box::new(scripted.clone()));
    runtime
        .add(RunSpec {
            id: RUN.to_string(),
            definition: parse(text).unwrap(),
            attempt_dir: temp.path().join("runs/1/attempts/1"),
            loop_bound: 5,
        })
        .unwrap();
    (runtime, scripted)
}

fn key(node: &str) -> NodeKey {
    NodeKey::new(node, 1)
}

fn item(node: &str, index: u32) -> NodeKey {
    NodeKey::new(node, 1).in_branch("split", index)
}

fn started(runtime: &Runtime<FakeRunner>, node: &str) -> usize {
    runtime
        .events(RUN)
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::NodeStarted { key, .. } if key.node == node))
        .count()
}

fn waiting(runtime: &Runtime<FakeRunner>) -> Vec<String> {
    runtime
        .pool_waiting(RUN)
        .into_iter()
        .map(|(key, _)| key.node)
        .collect()
}

#[test]
fn a_waiting_key_appends_nothing_and_starts_once_admitted() {
    let temp = TempDir::new().unwrap();
    let (mut runtime, scripted) = runtime(&temp, SINGLE, Limits::default());
    scripted.wait("work");
    runtime.tick();
    assert_eq!(
        started(&runtime, "work"),
        0,
        "no NodeStarted while it waits"
    );
    assert!(
        runtime.runner().started.is_empty(),
        "the runner was never asked"
    );
    assert_eq!(runtime.sessions_in_flight(), 0);
    assert_eq!(waiting(&runtime), ["work"]);
    assert!(!runtime.is_queued(RUN), "parked: it holds no run slot");
    // Nothing changes while it still waits: the loop does not spin.
    assert!(!runtime.tick());
    assert!(scripted.admits("work") >= 2, "asked again at each start");

    scripted.go("work");
    assert!(runtime.tick());
    assert_eq!(started(&runtime, "work"), 1);
    assert_eq!(runtime.runner().started.len(), 1);
    assert!(waiting(&runtime).is_empty());
    assert_eq!(scripted.told(), ["started work"]);

    runtime.runner_mut().release_all();
    runtime.tick();
    assert_eq!(scripted.told(), ["finished work", "ended"]);
    assert!(matches!(
        runtime.state(RUN).unwrap().ended,
        Some((EndState::Succeeded, _))
    ));
}

#[test]
fn a_refused_key_fails_like_a_start_failure() {
    let temp = TempDir::new().unwrap();
    let (mut runtime, scripted) = runtime(&temp, SINGLE, Limits::default());
    scripted.answer(
        "work",
        Admission::Refused("pool `gpu` is not declared in pools/pools.toml".to_string()),
    );
    runtime.tick();
    assert!(runtime.runner().started.is_empty());
    let events = runtime.events(RUN).unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Event::NodeStarted { key, pid: 0, .. } if *key == self::key("work")
    )));
    let failure = events
        .iter()
        .find_map(|event| match event {
            Event::NodeFinished {
                failure: Some(failure),
                ..
            } => Some(failure.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(failure.reason, FailureReason::PoolRefused);
    assert_eq!(
        failure.detail,
        "pool `gpu` is not declared in pools/pools.toml"
    );
    assert_eq!(started(&runtime, "work"), 1, "never retried");
    assert!(matches!(
        runtime.state(RUN).unwrap().ended,
        Some((EndState::Failed, _))
    ));
    assert_eq!(scripted.told(), ["finished work", "ended"]);
}

#[test]
fn a_start_that_fails_after_go_gives_back_what_it_was_let_have() {
    let temp = TempDir::new().unwrap();
    let (mut runtime, scripted) = runtime(&temp, SINGLE, Limits::default());
    // An unbound run cannot be asked about running unsandboxed: the start is refused.
    runtime.runner_mut().no_sandbox.insert("work".to_string());
    runtime.tick();
    let told = scripted.told();
    assert!(!told.contains(&"started work".to_string()), "{told:?}");
    assert_eq!(told.first().map(String::as_str), Some("finished work"));
}

#[test]
fn halting_releases_and_resuming_asks_again() {
    let temp = TempDir::new().unwrap();
    let (mut runtime, scripted) = runtime(&temp, SINGLE, Limits::default());
    runtime.tick();
    assert_eq!(scripted.told(), ["started work"]);
    runtime.halt(RUN).unwrap();
    assert_eq!(scripted.told(), ["finished work", "waiting"]);

    // The pool is busy at the resume: the key waits, nothing is appended for it, and the run is
    // not held (it waits for its pool, not for a person).
    scripted.wait("work");
    runtime.resume(RUN).unwrap();
    runtime.tick();
    assert_eq!(started(&runtime, "work"), 1);
    assert_eq!(waiting(&runtime), ["work"]);
    assert_eq!(runtime.sessions_in_flight(), 0);
    assert!(!runtime.tick(), "waiting changes nothing");

    scripted.go("work");
    runtime.tick();
    assert_eq!(
        started(&runtime, "work"),
        2,
        "the resume's try starts once admitted"
    );
    assert_eq!(runtime.in_flight(RUN), [key("work")]);
    assert_eq!(scripted.told(), ["started work"]);
}

#[test]
fn resuming_an_interrupted_run_waits_for_the_pool_then_starts() {
    let temp = TempDir::new().unwrap();
    let (mut runtime, scripted) = runtime(&temp, SINGLE, Limits::default());
    runtime.tick();
    assert!(runtime.interrupt_all());
    assert_eq!(
        scripted.told(),
        ["started work", "finished work", "waiting"]
    );
    scripted.wait("work");
    runtime.resume(RUN).unwrap();
    runtime.tick();
    assert_eq!(waiting(&runtime), ["work"]);
    assert!(!runtime.state(RUN).unwrap().interrupted.is_empty());
    scripted.go("work");
    runtime.tick();
    assert_eq!(started(&runtime, "work"), 2);
    assert!(runtime.state(RUN).unwrap().interrupted.is_empty());
}

#[test]
fn halting_cancelling_or_interrupting_drops_the_waiting_keys() {
    for stop in ["halt", "cancel", "interrupt"] {
        let temp = TempDir::new().unwrap();
        let (mut runtime, scripted) = runtime(&temp, SINGLE, Limits::default());
        scripted.wait("work");
        runtime.tick();
        assert_eq!(waiting(&runtime), ["work"], "{stop}");
        match stop {
            "halt" => runtime.halt(RUN).unwrap(),
            "cancel" => runtime.cancel(RUN).unwrap(),
            _ => {
                runtime.interrupt_all();
            }
        }
        assert!(waiting(&runtime).is_empty(), "{stop}");
        let told = scripted.told();
        let expected = if stop == "cancel" { "ended" } else { "waiting" };
        assert_eq!(told, [expected], "{stop}");
    }
}

#[test]
fn a_waiting_key_holds_no_session_and_its_sibling_starts() {
    let temp = TempDir::new().unwrap();
    let limits = Limits {
        max_concurrent_sessions: Some(1),
        ..Limits::default()
    };
    let (mut runtime, scripted) = runtime(&temp, PAIR, limits);
    scripted.wait("a");
    runtime.tick();
    runtime.runner_mut().release_all();
    runtime.tick();
    assert_eq!(waiting(&runtime), ["a"]);
    assert_eq!(runtime.in_flight(RUN), [item("b", 1)]);
    assert_eq!(runtime.sessions_in_flight(), 1, "within the one slot");

    // Granted while `b` holds the only slot: it waits for the slot, not past it.
    scripted.go("a");
    runtime.tick();
    assert_eq!(runtime.sessions_in_flight(), 1);
    assert_eq!(started(&runtime, "a"), 0);
    runtime.runner_mut().release_all();
    runtime.tick();
    assert_eq!(started(&runtime, "a"), 1);
}

#[test]
fn a_gate_in_one_branch_releases_nothing_of_another_branchs_running_node() {
    let gated = PAIR.replace(
        "[nodes.a]\nkind = \"agent\"\n",
        "[nodes.a]\nkind = \"agent\"\ngate = { when = \"before\", question = \"Go?\" }\n",
    );
    let temp = TempDir::new().unwrap();
    let (mut runtime, scripted) = runtime(&temp, &gated, Limits::default());
    runtime.tick();
    runtime.runner_mut().release_all();
    runtime.tick();
    let state = runtime.state(RUN).unwrap();
    assert!(state.open_gates.contains_key(&item("a", 0)), "{state:?}");
    assert_eq!(runtime.in_flight(RUN), [item("b", 1)]);
    let told = scripted.told();
    assert!(
        !told
            .iter()
            .any(|call| call == "finished b" || call == "waiting"),
        "{told:?}"
    );
}

#[test]
fn a_paused_run_drops_its_waiting_keys() {
    let temp = TempDir::new().unwrap();
    let (mut runtime, scripted) = runtime(&temp, PAIR, Limits::default());
    scripted.wait("a");
    runtime.tick();
    runtime.runner_mut().release_all();
    runtime.tick();
    assert_eq!(waiting(&runtime), ["a"]);
    scripted.told();
    runtime.pause(RUN).unwrap();
    runtime.tick();
    assert!(waiting(&runtime).is_empty());
    assert_eq!(scripted.told(), ["finished a"]);
}

#[test]
fn with_no_admission_set_every_start_goes() {
    let temp = TempDir::new().unwrap();
    let mut runtime = Runtime::new(Limits::default(), FakeRunner::new());
    assert!(!runtime.has_admission());
    runtime
        .add(RunSpec {
            id: RUN.to_string(),
            definition: parse(SINGLE).unwrap(),
            attempt_dir: temp.path().join("runs/1/attempts/1"),
            loop_bound: 5,
        })
        .unwrap();
    runtime.tick();
    assert_eq!(runtime.in_flight(RUN), [key("work")]);
    assert!(runtime.pool_waiting(RUN).is_empty());
}

#[test]
fn with_no_needs_validator_a_node_with_needs_is_refused() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("single");
    fs::create_dir_all(&dir).unwrap();
    let text = SINGLE.replace(
        "kind = \"agent\"\n",
        "kind = \"agent\"\nprompt = \"p\"\nneeds = [\"gpu\"]\n",
    );
    fs::write(dir.join(DEFINITION_FILE), &text).unwrap();
    fs::write(dir.join("p"), "Work.").unwrap();
    let resolved = Resolved {
        name: "single".to_string(),
        layer: Layer::Profile,
        dir,
        overrides: Vec::new(),
    };
    let (report, definition) =
        crate::definitions::check_parsed_in(&resolved, ValidationContext::default(), |_, _| {});
    let definition = definition.unwrap();
    assert_eq!(
        definition.nodes[0]
            .needs
            .as_ref()
            .map(|needs| needs.value.clone()),
        Some(toml::Value::Array(vec![toml::Value::String(
            "gpu".to_string()
        )]))
    );
    assert!(
        definition.warnings().is_empty(),
        "`needs` is a known node key"
    );
    let line = text
        .lines()
        .position(|line| line.starts_with("needs"))
        .unwrap()
        + 1;
    let refused: Vec<(usize, &str)> = report
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .map(|diagnostic| (diagnostic.line, diagnostic.message.as_str()))
        .collect();
    assert_eq!(refused, [(line, NEEDS_NOT_AVAILABLE)]);
    assert_eq!(
        NEEDS_NOT_AVAILABLE,
        "`needs` is not available (pools are off: pools.enabled)"
    );
}
