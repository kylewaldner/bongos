//! The app's node runner: [`AppRunner`] picks the node kind that runs each node (the agent kind,
//! the program kind, or a built-in) and fans polling and stopping out to every kind. A kind not
//! plugged in yet is [`Slot::Unwired`] and fails its nodes the way the engine's stand-in runner
//! does.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use bongos_workflow_model::definition::{AgentKind, NodeKind as DefinitionKind};
use bongos_workflow_model::{Failure, FailureReason, NodeKey};

use crate::dispatch_agent::AgentNodes;
use crate::engine::{UNWIRED_DETAIL, unwired_failure};
use crate::lifecycle::agent_of;
use crate::process::ProcessRecord;
use crate::runner_claude::{AgentDefaults, ClaudeCli};
use crate::runtime::{Finish, NodeStart, RunBinding, Runner};
use bongos_workflow_model::scheduler::Branch;

/// What the app knows that node kinds need, read when a node starts: the agent defaults, the
/// Claude CLI check, the MCP servers, `mcp_exclude` and the base environment. The program settings,
/// the interpreter and the remote hosts join it with their kinds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunnerSnapshot {
    /// `workflows.agents`.
    pub agents: AgentDefaults,
    /// The Claude CLI check ([`ClaudeCli::check`]); `None` until the app has run it.
    pub claude: Option<Result<ClaudeCli, Failure>>,
    /// The MCP servers discovered for each agent CLI.
    pub mcp_installed: BTreeMap<AgentKind, BTreeSet<String>>,
    /// `workflows.mcp_exclude`.
    pub mcp_exclude: Vec<String>,
    /// The environment agent processes take `PATH`, `HOME`, `LANG` and `TMPDIR` from.
    pub base_env: BTreeMap<String, String>,
}

/// A shared, replaceable [`RunnerSnapshot`]: the app sets it, the runner reads a copy.
#[derive(Debug, Clone, Default)]
pub struct RunnerInputs(Arc<Mutex<RunnerSnapshot>>);

impl RunnerInputs {
    pub fn new(snapshot: RunnerSnapshot) -> Self {
        Self(Arc::new(Mutex::new(snapshot)))
    }

    pub fn set(&self, snapshot: RunnerSnapshot) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
    }

    pub fn snapshot(&self) -> RunnerSnapshot {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One kind of node: how its tries start, end and stop.
pub(crate) trait NodeKind {
    fn start(
        &mut self,
        node: &NodeStart<'_>,
        binding: &RunBinding,
        inputs: &RunnerSnapshot,
    ) -> Result<ProcessRecord, Failure>;

    /// The tries that ended since the last call.
    fn poll(&mut self) -> Vec<Finish>;

    fn stop(&mut self, tries: &[(String, NodeKey)]);
}

/// A place for a node kind.
#[derive(Default)]
pub(crate) enum Slot {
    /// Nothing plugged in: its nodes fail.
    #[default]
    Unwired,
    #[allow(dead_code)] // a later step plugs the kinds in
    Wired(Box<dyn NodeKind>),
}

/// The node kinds an [`AppRunner`] dispatches to.
#[derive(Default)]
pub(crate) struct Kinds {
    agent: Slot,
    program: Slot,
}

impl Kinds {
    pub(crate) fn set_agent(&mut self, kind: Box<dyn NodeKind>) {
        self.agent = Slot::Wired(kind);
    }

    #[allow(dead_code)] // a later step plugs the kinds in
    pub(crate) fn set_program(&mut self, kind: Box<dyn NodeKind>) {
        self.program = Slot::Wired(kind);
    }

    fn wired(&mut self) -> impl Iterator<Item = &mut Box<dyn NodeKind>> {
        [&mut self.agent, &mut self.program]
            .into_iter()
            .filter_map(|slot| match slot {
                Slot::Wired(kind) => Some(kind),
                Slot::Unwired => None,
            })
    }
}

/// The runner of the app: dispatches each node to its kind.
#[derive(Default)]
pub struct AppRunner {
    kinds: Kinds,
    bindings: BTreeMap<String, RunBinding>,
    inputs: RunnerInputs,
    connected: bool,
}

impl AppRunner {
    /// A runner with the agent kind plugged in.
    pub fn new(inputs: RunnerInputs) -> Self {
        let mut runner = Self {
            inputs,
            ..Self::default()
        };
        runner.kinds.set_agent(Box::new(AgentNodes::default()));
        runner
    }

    #[allow(dead_code)] // a later step plugs the kinds in
    pub(crate) fn kinds_mut(&mut self) -> &mut Kinds {
        &mut self.kinds
    }

    pub fn inputs(&self) -> &RunnerInputs {
        &self.inputs
    }

    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    fn run_in(
        slot: &mut Slot,
        node: &NodeStart<'_>,
        bindings: &BTreeMap<String, RunBinding>,
        inputs: &RunnerInputs,
    ) -> Result<ProcessRecord, Failure> {
        let Slot::Wired(kind) = slot else {
            return Err(unwired_failure(node.key));
        };
        let binding = node
            .binding
            .or_else(|| bindings.get(node.run))
            .ok_or_else(|| {
                Failure::new(
                    FailureReason::WorktreeUnavailable,
                    Some(node.key.clone()),
                    format!("run {} has no worktree bound", node.run),
                )
            })?;
        kind.start(node, binding, &inputs.snapshot())
    }
}

impl Runner for AppRunner {
    fn is_connected(&self) -> bool {
        self.connected
    }

    fn start(
        &mut self,
        _run: &str,
        _definition: &bongos_workflow_model::Definition,
        key: &NodeKey,
        _try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        Err(unwired_failure(key))
    }

    fn bind(&mut self, run: &str, binding: RunBinding) {
        self.bindings.insert(run.to_string(), binding);
    }

    fn unbind(&mut self, run: &str) {
        self.bindings.remove(run);
    }

    fn start_node(&mut self, node: NodeStart<'_>) -> Result<ProcessRecord, Failure> {
        let fail = |reason, detail: String| Failure::new(reason, Some(node.key.clone()), detail);
        let Some(definition_node) = node.definition.node(&node.key.node) else {
            return Err(fail(
                FailureReason::NodeOutputInvalid,
                format!("the definition has no node '{}'", node.key.node),
            ));
        };
        match definition_node.kind.value {
            DefinitionKind::Agent => match agent_of(node.definition, definition_node) {
                AgentKind::Claude => {
                    Self::run_in(&mut self.kinds.agent, &node, &self.bindings, &self.inputs)
                }
                AgentKind::Codex => Err(fail(
                    FailureReason::AgentNotInstalledOrLoggedOut,
                    "Codex nodes are not supported yet".to_string(),
                )),
            },
            DefinitionKind::Program => match &definition_node.use_node {
                Some(name) => Err(fail(
                    FailureReason::ProgramInterpreterMissing,
                    format!("built-in `{}` is not registered", name.value),
                )),
                None => Self::run_in(&mut self.kinds.program, &node, &self.bindings, &self.inputs),
            },
        }
    }

    fn poll(&mut self) -> Vec<Finish> {
        self.kinds.wired().flat_map(|kind| kind.poll()).collect()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        for kind in self.kinds.wired() {
            kind.stop(tries);
        }
    }

    fn merge_branch(
        &mut self,
        _run: &str,
        _join: &str,
        _branch: &Branch,
    ) -> Result<String, Failure> {
        Err(Failure::new(
            FailureReason::MergeConflict,
            None,
            UNWIRED_DETAIL,
        ))
    }
}
