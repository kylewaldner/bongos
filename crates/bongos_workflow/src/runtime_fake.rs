use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};

use bongos_workflow_model::unsandboxed::{Launch, Mode, Permit, decide};
use bongos_workflow_model::{Definition, Failure, FailureReason, NodeKey};
use serde_json::{Value, json};

use super::{Finish, NodeStart, Runner};
use crate::dispatch_program::program_permit;

/// What the fake's probe says when a `no_sandbox` node starts.
pub const NO_SANDBOX_REASON: &str = "bwrap not found";
use crate::process::{NodeProcess, ProcessRecord, stop_all};
use bongos_workflow_model::scheduler::Branch;

struct Try {
    run: String,
    key: NodeKey,
    try_number: u32,
    process: Option<NodeProcess>,
    released: bool,
}

/// A scripted [`Runner`] for tests. Each start takes the node's scripted result (or the default
/// `{"summary": "ok"}`); a try finishes when the test releases it, or at the next poll with
/// `auto_finish`. With `command` set, each start spawns that command as a real process in its own
/// session ([`NodeProcess`]), so halt, cancel and quit stop real processes.
#[derive(Default)]
pub struct FakeRunner {
    /// Finish every try at the next poll.
    pub auto_finish: bool,
    /// A command each start spawns as a real process.
    pub command: Option<Vec<String>>,
    /// Every try started, in order, as `(run, key)`.
    pub started: Vec<(String, NodeKey)>,
    /// Every try stopped, in order.
    pub stopped: Vec<(String, NodeKey)>,
    /// The processes stopped, kept so a test can check they are gone.
    pub stopped_processes: Vec<NodeProcess>,
    /// The most tries in flight at once, across runs.
    pub max_in_flight: usize,
    /// Every merge fails `merge conflict` when set, after the first `clean_merges` merges.
    pub merge_conflicts: bool,
    /// How many merges succeed before `merge_conflicts` takes effect.
    pub clean_merges: usize,
    /// Every merge asked for, as `(run, join, branch)`.
    pub merges: Vec<(String, String, Branch)>,
    /// The `budget_left_usd` of every start, in order.
    pub budgets: Vec<Option<f64>>,
    /// Nodes whose start finds no OS sandbox ([`NO_SANDBOX_REASON`]): each start decides as the
    /// program node kind does under `ask`, from the run's facts and the start's grant.
    pub no_sandbox: BTreeSet<String>,
    /// The permit of every start of a `no_sandbox` node, in order.
    pub permits: Vec<(NodeKey, Permit)>,
    /// The mode the run's built-ins start under ([`Runner::unsandboxed_mode`]).
    pub unsandboxed_mode: Mode,
    results: BTreeMap<String, Value>,
    /// The cost each node's tries report.
    costs: BTreeMap<String, f64>,
    /// Tries of each node still to fail, `agent exited nonzero`.
    failures: BTreeMap<String, u32>,
    /// The session id each node's tries report.
    sessions: BTreeMap<String, String>,
    running: Vec<Try>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// The result every try of `node` finishes with.
    pub fn script(&mut self, node: &str, result: Value) -> &mut Self {
        self.results.insert(node.to_string(), result);
        self
    }

    /// The cost every try of `node` reports when it finishes.
    pub fn cost(&mut self, node: &str, usd: f64) -> &mut Self {
        self.costs.insert(node.to_string(), usd);
        self
    }

    /// The session id every try of `node` reports when it finishes.
    pub fn session(&mut self, node: &str, id: &str) -> &mut Self {
        self.sessions.insert(node.to_string(), id.to_string());
        self
    }

    /// The next `times` tries of `node` fail `agent exited nonzero`.
    pub fn fail(&mut self, node: &str, times: u32) -> &mut Self {
        self.failures.insert(node.to_string(), times);
        self
    }

    /// The tries in flight, as `(run, key)`.
    pub fn in_flight(&self) -> Vec<(String, NodeKey)> {
        self.running
            .iter()
            .map(|running| (running.run.clone(), running.key.clone()))
            .collect()
    }

    /// The processes of the tries in flight.
    pub fn processes(&mut self) -> impl Iterator<Item = &mut NodeProcess> {
        self.running
            .iter_mut()
            .filter_map(|running| running.process.as_mut())
    }

    /// Lets every try in flight finish at the next poll.
    pub fn release_all(&mut self) {
        for running in &mut self.running {
            running.released = true;
        }
    }

    /// Lets the tries of node `node` of run `run` finish at the next poll.
    pub fn release_node(&mut self, run: &str, node: &str) {
        for running in &mut self.running {
            if running.run == run && running.key.node == node {
                running.released = true;
            }
        }
    }

    /// Lets the tries of run `run` finish at the next poll.
    pub fn release(&mut self, run: &str) {
        for running in &mut self.running {
            if running.run == run {
                running.released = true;
            }
        }
    }
}

impl Runner for FakeRunner {
    fn start(
        &mut self,
        run: &str,
        _definition: &Definition,
        key: &NodeKey,
        try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        let process = self.command.as_ref().map(|argv| {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            NodeProcess::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
                .expect("the fake command starts")
        });
        let record = process.as_ref().map_or(
            ProcessRecord {
                pid: 4000 + self.started.len() as u32,
                start_time: 0,
            },
            NodeProcess::record,
        );
        self.started.push((run.to_string(), key.clone()));
        self.running.push(Try {
            run: run.to_string(),
            key: key.clone(),
            try_number,
            process,
            released: false,
        });
        self.max_in_flight = self.max_in_flight.max(self.running.len());
        Ok(record)
    }

    fn unsandboxed_mode(&self, _run: &str) -> Mode {
        self.unsandboxed_mode
    }

    fn start_node(&mut self, node: NodeStart<'_>) -> Result<ProcessRecord, Failure> {
        self.budgets.push(node.budget_left_usd);
        if self.no_sandbox.contains(&node.key.node) {
            // As the program node kind decides it under `ask`, with a probe that finds no sandbox.
            let facts = node
                .binding
                .map(|binding| binding.setup.facts)
                .unwrap_or_default();
            let permit = program_permit(&facts, Mode::Ask, node.grant.as_ref());
            self.permits.push((node.key.clone(), permit.clone()));
            let availability = Err(NO_SANDBOX_REASON.to_string());
            let fail = |reason, message| Err(Failure::new(reason, Some(node.key.clone()), message));
            match decide(&availability, &permit, Mode::Ask) {
                Launch::Unsandboxed { .. } | Launch::Sandboxed => {}
                Launch::Ask { message } => return fail(FailureReason::SandboxRequired, message),
                Launch::Refuse { message } => return fail(FailureReason::SandboxRefused, message),
            }
        }
        self.start(node.run, node.definition, node.key, node.try_number)
    }

    fn poll(&mut self) -> Vec<Finish> {
        let auto_finish = self.auto_finish;
        let (done, running): (Vec<Try>, Vec<Try>) = std::mem::take(&mut self.running)
            .into_iter()
            .partition(|running| auto_finish || running.released);
        self.running = running;
        done.into_iter()
            .map(|mut done| {
                if let Some(process) = done.process.as_mut() {
                    process.stop();
                }
                let cost_usd = self.costs.get(&done.key.node).copied().unwrap_or(0.0);
                if let Some(left) = self.failures.get_mut(&done.key.node)
                    && *left > 0
                {
                    *left -= 1;
                    let failure = Failure::new(
                        FailureReason::AgentExitedNonzero,
                        Some(done.key.clone()),
                        "scripted failure",
                    );
                    return Finish {
                        outcome: Err(failure),
                        cost_usd,
                        ..Finish::ok(done.run, done.key, done.try_number, Value::Null)
                    };
                }
                let result = self
                    .results
                    .get(&done.key.node)
                    .cloned()
                    .unwrap_or_else(|| json!({ "summary": "ok" }));
                let session_id = self.sessions.get(&done.key.node).cloned();
                Finish {
                    session_id,
                    cost_usd,
                    ..Finish::ok(done.run, done.key, done.try_number, result)
                }
            })
            .collect()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        let (stopped, running): (Vec<Try>, Vec<Try>) = std::mem::take(&mut self.running)
            .into_iter()
            .partition(|running| {
                tries
                    .iter()
                    .any(|(run, key)| *run == running.run && *key == running.key)
            });
        self.running = running;
        let mut processes = Vec::new();
        for stopped in stopped {
            self.stopped.push((stopped.run, stopped.key));
            processes.extend(stopped.process);
        }
        stop_all(&mut processes);
        self.stopped_processes.extend(processes);
    }

    fn merge_branch(&mut self, run: &str, join: &str, branch: &Branch) -> Result<String, Failure> {
        self.merges
            .push((run.to_string(), join.to_string(), branch.clone()));
        if self.merge_conflicts && self.merges.len() > self.clean_merges {
            return Err(Failure::new(
                FailureReason::MergeConflict,
                None,
                "conflict in src/lib.rs",
            ));
        }
        Ok("0000000".to_string())
    }
}
