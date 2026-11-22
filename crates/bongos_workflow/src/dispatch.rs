//! The app's node runner: [`AppRunner`] picks the node kind that runs each node (the agent kind,
//! the program kind, or a built-in) and fans polling and stopping out to every kind. A kind not
//! plugged in fails its nodes. The engine builds one and refreshes its [`RunnerInputs`] before every tick.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use bongos_workflow_model::definition::{AgentKind, NodeKind as DefinitionKind};
use bongos_workflow_model::{Failure, FailureReason, NodeKey};

use crate::agent_confinement::AgentConfinementSettings;
use crate::dispatch_agent::AgentNodes;
use crate::dispatch_program::ProgramNodes;
use crate::lifecycle::agent_of;
use crate::process::ProcessRecord;
use crate::runner_claude::{AgentDefaults, ClaudeCli};
use crate::runner_program::{Interpreter, ProgramSettings};
use crate::runtime::{Finish, NodeStart, RunBinding, Runner};
use crate::worktrees::{self, Git, GitError, RunWorktree, branch_worktree_path};
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
    /// The program settings (`workflows.programs.*` and the poll settings).
    pub programs: ProgramSettings,
    /// The Python check ([`Interpreter::find`]); `None` until the app has run it.
    pub interpreter: Option<Result<Interpreter, Failure>>,
    /// The hosts `@remote` stands for, by run id; a run with no entry takes them from the task
    /// repository's push remote when its node starts.
    pub remote_hosts: BTreeMap<String, Vec<String>>,
    /// `confinement.agent_steps` and `confinement.agent_state_paths`.
    pub agent_confinement: AgentConfinementSettings,
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

/// One kind of node: how its tries start, end and stop. `Send`, since the engine's runner is.
pub(crate) trait NodeKind: Send {
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

/// The node kinds an [`AppRunner`] dispatches to; a kind not plugged in fails its nodes.
#[derive(Default)]
pub(crate) struct Kinds {
    agent: Option<Box<dyn NodeKind>>,
    program: Option<Box<dyn NodeKind>>,
}

impl Kinds {
    pub(crate) fn set_agent(&mut self, kind: Box<dyn NodeKind>) {
        self.agent = Some(kind);
    }

    pub(crate) fn set_program(&mut self, kind: Box<dyn NodeKind>) {
        self.program = Some(kind);
    }

    fn wired(&mut self) -> impl Iterator<Item = &mut Box<dyn NodeKind>> {
        [&mut self.agent, &mut self.program]
            .into_iter()
            .filter_map(Option::as_mut)
    }
}

/// The runner of the app: dispatches each node to its kind.
pub struct AppRunner {
    kinds: Kinds,
    bindings: BTreeMap<String, RunBinding>,
    inputs: RunnerInputs,
    /// The git that merges fan-out branches at their joins.
    git: Git,
}

impl Default for AppRunner {
    fn default() -> Self {
        Self {
            kinds: Kinds::default(),
            bindings: BTreeMap::new(),
            inputs: RunnerInputs::default(),
            git: Git::new("git"),
        }
    }
}

impl AppRunner {
    /// A runner with the agent and program kinds plugged in (the built-ins run through the
    /// runtime's [`crate::BuiltinNodes`]).
    pub fn new(inputs: RunnerInputs) -> Self {
        let mut runner = Self {
            inputs,
            ..Self::default()
        };
        runner.kinds.set_agent(Box::new(AgentNodes::default()));
        runner.kinds.set_program(Box::new(ProgramNodes::default()));
        runner
    }

    #[cfg(test)]
    pub(crate) fn kinds_mut(&mut self) -> &mut Kinds {
        &mut self.kinds
    }

    /// Replaces the git that merges fan-out branches, so tests can isolate it.
    #[cfg(test)]
    pub(crate) fn set_git(&mut self, git: Git) {
        self.git = git;
    }

    pub fn inputs(&self) -> &RunnerInputs {
        &self.inputs
    }

    fn run_in(
        slot: &mut Option<Box<dyn NodeKind>>,
        node: &NodeStart<'_>,
        bindings: &BTreeMap<String, RunBinding>,
        inputs: &RunnerInputs,
    ) -> Result<ProcessRecord, Failure> {
        let Some(kind) = slot else {
            return Err(Failure::new(
                FailureReason::AgentNotInstalledOrLoggedOut,
                Some(node.key.clone()),
                "no runner is plugged in for this kind of node",
            ));
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
    fn start(
        &mut self,
        run: &str,
        _definition: &bongos_workflow_model::Definition,
        key: &NodeKey,
        _try_number: u32,
    ) -> Result<ProcessRecord, Failure> {
        // The runtime starts every node through `start_node`, which has the run's binding.
        Err(Failure::new(
            FailureReason::WorktreeUnavailable,
            Some(key.clone()),
            format!("run {run} has no worktree bound"),
        ))
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

    fn merge_branch(&mut self, run: &str, join: &str, branch: &Branch) -> Result<String, Failure> {
        let Some(binding) = self.bindings.get(run) else {
            return Err(Failure::new(
                FailureReason::WorktreeUnavailable,
                None,
                format!("run {run} has no worktree bound"),
            ));
        };
        // A branch's worktree is made for its innermost fan (`dispatch_agent::node_worktree`).
        let Some((fan, index)) = branch.last() else {
            return Err(Failure::new(
                FailureReason::MergeConflict,
                None,
                format!("join '{join}' was handed an empty branch"),
            ));
        };
        merge_into_run(&self.git, &binding.worktree, fan, *index)
    }
}

/// Merges fan-out branch `index` of `fan` into the run branch with [`worktrees::join`], in the
/// run worktree, and returns the run branch's head. A branch that never got a worktree (none of
/// its nodes ran in one) has nothing to merge. A conflict is aborted, leaving every branch as it
/// was, and fails naming the files.
fn merge_into_run(git: &Git, run: &RunWorktree, fan: &str, index: u32) -> Result<String, Failure> {
    let git_failure =
        |error: GitError| Failure::new(FailureReason::WorktreeUnavailable, None, error.to_string());
    if branch_worktree_path(&run.run_dir, fan, index).exists() {
        let outcome = futures::executor::block_on(worktrees::join(git, run, fan, &[index]))
            .map_err(git_failure)?;
        if let Some((_, files)) = outcome.conflict {
            return Err(Failure::new(
                FailureReason::MergeConflict,
                None,
                format!("merge conflict in {}", files.join(", ")),
            ));
        }
    }
    futures::executor::block_on(git.run_at(run.at(), &["rev-parse", "HEAD"])).map_err(git_failure)
}
