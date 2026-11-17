//! Tests of the runner seam: the context a runtime gives its runner (`bind`, `unbind`,
//! `start_node`) and the app runner's dispatch of nodes to their kinds. No process runs.

use std::sync::{Arc, Mutex};

use bongos_workflow_model::{Definition, Failure, FailureReason, Limits, NodeKey, RunState, parse};
use tempfile::TempDir;

use crate::dispatch::{AppRunner, NodeKind, RunnerInputs, RunnerSnapshot};
use crate::engine::UNWIRED_DETAIL;
use crate::lifecycle::StartChecks;
use crate::process::ProcessRecord;
use crate::runtime::{Finish, NodeStart, RunBinding, RunSpec, Runner, Runtime};
use crate::worktrees::{RunWorktree, TaskCheckout};

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

const MIXED: &str = r#"
schema = 1
name = "mixed"
start = "claude"

[nodes.claude]
kind = "agent"
output = { summary = { type = "text" } }
routes = [{ to = "codex" }]

[nodes.codex]
kind = "agent"
agent = "codex"
output = { summary = { type = "text" } }
routes = [{ to = "script" }]

[nodes.script]
kind = "program"
script = "run.py"
output = { summary = { type = "text" } }
routes = [{ to = "shipped" }]

[nodes.shipped]
kind = "program"
use = "ghost"
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

pub(crate) fn binding(root: &std::path::Path, workflow: &str) -> RunBinding {
    RunBinding {
        workflow: workflow.to_string(),
        task_id: "task".to_string(),
        task_dir: root.join("task"),
        run_dir: root.join("task/runs/r"),
        attempt_dir: root.join("task/runs/r/attempts/1"),
        worktree: RunWorktree {
            run_dir: root.join("wt"),
            path: root.join("wt/tree"),
            branch: "branch-1".to_string(),
        },
        checkout: TaskCheckout {
            slug: "task".to_string(),
            worktree: None,
            branch: None,
        },
        inputs: Vec::new(),
    }
}

/// A runner that records the calls it gets, in order, in a shared log.
#[derive(Clone, Default)]
pub(crate) struct Recorder {
    pub log: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    pub fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn push(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
}

impl Runner for Recorder {
    fn start(
        &mut self,
        _run: &str,
        _definition: &Definition,
        key: &NodeKey,
        _try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        self.push(format!("start {}", key.node));
        Ok(ProcessRecord {
            pid: 1,
            start_time: 0,
        })
    }

    fn bind(&mut self, run: &str, binding: RunBinding) {
        self.push(format!("bind {run} {}", binding.worktree.path.display()));
    }

    fn unbind(&mut self, run: &str) {
        self.push(format!("unbind {run}"));
    }

    fn start_node(&mut self, node: NodeStart<'_>) -> Result<ProcessRecord, Failure> {
        let tree = node.binding.map_or("unbound".to_string(), |b| {
            b.worktree.path.display().to_string()
        });
        self.push(format!("start_node {} {tree}", node.key.node));
        Ok(ProcessRecord {
            pid: 1,
            start_time: 0,
        })
    }

    fn poll(&mut self) -> Vec<Finish> {
        Vec::new()
    }

    fn stop(&mut self, _tries: &[(String, NodeKey)]) {}

    fn merge_branch(
        &mut self,
        _run: &str,
        _join: &str,
        _branch: &bongos_workflow_model::scheduler::Branch,
    ) -> Result<String, Failure> {
        Ok(String::new())
    }
}

fn spec(root: &std::path::Path, n: u32, text: &str) -> RunSpec {
    RunSpec {
        id: run_id(n),
        definition: definition(text),
        attempt_dir: root.join(format!("runs/{}/attempts/1", run_id(n))),
    }
}

#[test]
fn a_bound_run_is_started_with_its_binding_after_the_bind_and_unbound_at_remove() {
    let temp = TempDir::new().unwrap();
    let recorder = Recorder::default();
    let mut runtime = Runtime::new(Limits::default(), recorder.clone());
    runtime.add(spec(temp.path(), 1, SINGLE)).unwrap();
    runtime.add(spec(temp.path(), 2, SINGLE)).unwrap();
    runtime.bind(&run_id(1), binding(temp.path(), "single"));
    runtime.tick();

    let lines = recorder.lines();
    let tree = temp.path().join("wt/tree");
    let bound = format!("bind {} {}", run_id(1), tree.display());
    assert_eq!(lines[0], bound);
    assert!(
        lines.contains(&format!("start_node work {}", tree.display())),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"start_node work unbound".to_string()),
        "a run that was never bound starts with no binding: {lines:?}"
    );

    assert!(runtime.remove(&run_id(1)));
    assert_eq!(
        recorder.lines().last().unwrap(),
        &format!("unbind {}", run_id(1))
    );
}

#[test]
fn start_checks_give_the_default_validation_context() {
    struct Checks;
    impl StartChecks for Checks {
        fn agent(&self, _kind: bongos_workflow_model::definition::AgentKind) -> Result<(), String> {
            Ok(())
        }
        fn interpreter(&self) -> Result<(), String> {
            Ok(())
        }
        fn mcp_servers(
            &self,
            _kind: bongos_workflow_model::definition::AgentKind,
        ) -> Option<std::collections::BTreeSet<String>> {
            None
        }
    }
    assert_eq!(
        Checks.validation(),
        bongos_workflow_model::validate::ValidationContext::default()
    );
}

/// A kind that records what it is asked.
#[derive(Clone, Default)]
struct KindLog {
    log: Arc<Mutex<Vec<String>>>,
}

struct Plugged {
    name: &'static str,
    log: KindLog,
}

impl NodeKind for Plugged {
    fn start(
        &mut self,
        node: &NodeStart<'_>,
        binding: &RunBinding,
        _inputs: &RunnerSnapshot,
    ) -> Result<ProcessRecord, Failure> {
        self.log.log.lock().unwrap().push(format!(
            "{} start {} {} {}",
            self.name, node.run, node.key.node, binding.workflow
        ));
        Ok(ProcessRecord {
            pid: 7,
            start_time: 0,
        })
    }

    fn poll(&mut self) -> Vec<Finish> {
        self.log
            .log
            .lock()
            .unwrap()
            .push(format!("{} poll", self.name));
        vec![Finish::ok(
            self.name,
            NodeKey::new("work", 1),
            1,
            serde_json::json!({}),
        )]
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        self.log
            .log
            .lock()
            .unwrap()
            .push(format!("{} stop {}", self.name, tries.len()));
    }
}

fn start(
    runner: &mut AppRunner,
    definition: &Definition,
    node: &str,
) -> Result<ProcessRecord, Failure> {
    let key = NodeKey::new(node, 1);
    let state = RunState::default();
    runner.start_node(NodeStart {
        run: "run",
        definition,
        key: &key,
        try_number: 1,
        state: &state,
        budget_left_usd: None,
        binding: None,
    })
}

#[test]
fn unwired_kinds_fail_agent_and_program_nodes_with_the_unwired_text() {
    let definition = definition(MIXED);
    let mut runner = AppRunner::default();
    for node in ["claude", "script"] {
        let failure = start(&mut runner, &definition, node).unwrap_err();
        assert_eq!(failure.detail, UNWIRED_DETAIL, "{node}");
        assert_eq!(failure.node, Some(NodeKey::new(node, 1)));
    }
}

#[test]
fn builtin_and_codex_nodes_fail_with_their_own_text() {
    let definition = definition(MIXED);
    let mut runner = AppRunner::default();
    let builtin = start(&mut runner, &definition, "shipped").unwrap_err();
    assert_eq!(builtin.detail, "built-in `ghost` is not registered");
    let codex = start(&mut runner, &definition, "codex").unwrap_err();
    assert_eq!(codex.reason, FailureReason::AgentNotInstalledOrLoggedOut);
    assert!(
        codex.detail.contains("not supported yet"),
        "{}",
        codex.detail
    );
}

#[test]
fn a_plugged_in_kind_gets_the_start_with_its_binding_and_polls_and_stops_fan_out() {
    let temp = TempDir::new().unwrap();
    let definition = definition(MIXED);
    let log = KindLog::default();
    let mut runner = AppRunner::new(RunnerInputs::default());
    runner.kinds_mut().set_agent(Box::new(Plugged {
        name: "agent",
        log: log.clone(),
    }));
    runner.kinds_mut().set_program(Box::new(Plugged {
        name: "program",
        log: log.clone(),
    }));

    let unbound = start(&mut runner, &definition, "claude").unwrap_err();
    assert_eq!(unbound.reason, FailureReason::WorktreeUnavailable);

    runner.bind("run", binding(temp.path(), "mixed"));
    assert_eq!(start(&mut runner, &definition, "claude").unwrap().pid, 7);
    assert_eq!(start(&mut runner, &definition, "script").unwrap().pid, 7);
    // The program node's unwired sibling kinds are not touched by a codex node.
    assert!(start(&mut runner, &definition, "codex").is_err());

    assert_eq!(runner.poll().len(), 2);
    runner.stop(&[("run".to_string(), NodeKey::new("claude", 1))]);
    assert_eq!(
        log.log.lock().unwrap().clone(),
        vec![
            "agent start run claude mixed",
            "program start run script mixed",
            "agent poll",
            "program poll",
            "agent stop 1",
            "program stop 1",
        ]
    );
}

#[test]
fn bind_and_unbind_track_bindings_and_connection_is_off_by_default() {
    let temp = TempDir::new().unwrap();
    let definition = definition(MIXED);
    let mut runner = AppRunner::default();
    assert!(!runner.connected());
    assert!(!Runner::is_connected(&runner));
    runner.set_connected(true);
    assert!(runner.connected());

    let log = KindLog::default();
    runner.kinds_mut().set_agent(Box::new(Plugged {
        name: "agent",
        log: log.clone(),
    }));
    runner.bind("run", binding(temp.path(), "mixed"));
    assert!(start(&mut runner, &definition, "claude").is_ok());
    runner.unbind("run");
    let failure = start(&mut runner, &definition, "claude").unwrap_err();
    assert_eq!(failure.reason, FailureReason::WorktreeUnavailable);
}

#[test]
fn runner_inputs_replace_and_copy_the_snapshot() {
    let inputs = RunnerInputs::default();
    let shared = inputs.clone();
    shared.set(RunnerSnapshot::default());
    assert_eq!(inputs.snapshot(), RunnerSnapshot::default());
}
