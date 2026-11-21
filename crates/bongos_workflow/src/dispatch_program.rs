//! The program node kind of the app runner: turns a node start into a [`ProgramNode`] (script of
//! the attempt's definition copy, caps, poll plan, state document, worktree, scratch, what it may
//! write, the host's record of its git and network list with `@remote` resolved) and hands it to
//! one [`ProgramRunner`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_confine::safe_git::linked_git_dir_from_record;
use bongos_confine::{HostGit, TreeAccess, TreeGit};
use bongos_workflow_model::record::TodoRef;
use bongos_workflow_model::state_doc::{self, Paths, RunIdentity};
use bongos_workflow_model::{Failure, FailureReason, NodeKey};

use crate::dispatch::{NodeKind, RunnerSnapshot};
use crate::dispatch_agent::node_worktree;
use crate::lifecycle::definition_copy_dir;
use crate::process::ProcessRecord;
use crate::run_view::try_dir;
use crate::runner_program::{
    Clock, Interpreter, PollPlan, ProgramCaps, ProgramNode, ProgramRunner, SandboxProbe,
};
use crate::runtime::{Finish, NodeStart, RunBinding};
use crate::worktrees::{Git, common_dir_of, repository_of};

/// The network entry that stands for the host of the task repository's push remote.
pub(crate) const REMOTE_ENTRY: &str = "@remote";

/// The failure text when `@remote` has no remote to take hosts from.
pub(crate) const NO_ORIGIN: &str = "the task repository has no origin remote to take hosts from";

/// Runs program nodes through one [`ProgramRunner`], built from the snapshot's interpreter.
#[derive(Default)]
pub(crate) struct ProgramNodes {
    runner: Option<ProgramRunner>,
    /// A clock and a sandbox probe for the runner, in place of the system's; tests set them.
    clock: Option<Arc<dyn Clock>>,
    probe: Option<SandboxProbe>,
}

impl ProgramNodes {
    #[cfg(test)]
    pub(crate) fn with_clock_and_probe(clock: Arc<dyn Clock>, probe: SandboxProbe) -> Self {
        Self {
            runner: None,
            clock: Some(clock),
            probe: Some(probe),
        }
    }

    /// The runner for `interpreter`; a runner with tries in flight or sleeping is kept, and
    /// replaced once it is idle and the interpreter changed.
    fn runner_for(
        &mut self,
        interpreter: Interpreter,
        snapshot: &RunnerSnapshot,
    ) -> &mut ProgramRunner {
        let stale = self.runner.as_ref().is_none_or(|runner| {
            runner.interpreter() != &interpreter
                && runner.in_flight().is_empty()
                && runner.sleeping().is_empty()
        });
        if stale {
            let mut runner = ProgramRunner::new(interpreter, snapshot.base_env.clone());
            if let Some(clock) = &self.clock {
                runner = runner.with_clock(clock.clone());
            }
            if let Some(probe) = &self.probe {
                runner = runner.with_sandbox_probe(probe.clone());
            }
            self.runner = Some(runner);
        }
        self.runner.get_or_insert_with(|| unreachable!())
    }
}

impl NodeKind for ProgramNodes {
    fn start(
        &mut self,
        node: &NodeStart<'_>,
        binding: &RunBinding,
        inputs: &RunnerSnapshot,
    ) -> Result<ProcessRecord, Failure> {
        let key = node.key;
        let fail = |reason, detail: String| Failure::new(reason, Some(key.clone()), detail);
        let interpreter = match &inputs.interpreter {
            Some(Ok(interpreter)) => interpreter.clone(),
            Some(Err(failure)) => {
                return Err(Failure::new(
                    failure.reason,
                    Some(key.clone()),
                    failure.detail.clone(),
                ));
            }
            None => {
                return Err(fail(
                    FailureReason::ProgramInterpreterMissing,
                    "the Python interpreter was not checked yet".to_string(),
                ));
            }
        };
        let definition_node = node.definition.node(&key.node).ok_or_else(|| {
            fail(
                FailureReason::NodeOutputInvalid,
                format!("the definition has no node '{}'", key.node),
            )
        })?;
        let script = definition_node.script.as_ref().ok_or_else(|| {
            fail(
                FailureReason::NodeOutputInvalid,
                format!("node '{}' has no script", key.node),
            )
        })?;
        let script =
            definition_copy_dir(&binding.attempt_dir, node.state.definition_rev.unwrap_or(0))
                .join(&script.value);
        let mut permissions = definition_node.permissions.clone().unwrap_or_default();
        if permissions
            .network
            .iter()
            .any(|entry| entry == REMOTE_ENTRY)
        {
            let hosts = match inputs.remote_hosts.get(node.run) {
                Some(hosts) => hosts.clone(),
                None => task_remote_hosts(&Git::new("git"), binding)
                    .map_err(|detail| fail(FailureReason::PermissionDenied, detail))?,
            };
            permissions.network = expand_remote(&permissions.network, &hosts);
        }
        let worktree = node_worktree(key, binding)
            .map_err(|detail| fail(FailureReason::WorktreeUnavailable, detail))?;
        let scratch = scratch_dir(binding, key, node.try_number);
        let state = state_doc::build(
            &binding.inputs,
            node.state,
            RunIdentity {
                id: node.run.to_string(),
                workflow: binding.workflow.clone(),
                todo: todo_of(binding),
                task_id: binding.task_id.clone(),
                task_dir: binding.task_dir.clone(),
                branch: binding.checkout.branch.clone().unwrap_or_default(),
                run_branch: binding.worktree.branch.clone(),
            },
            &definition_node.with,
            Paths {
                worktree: worktree.clone(),
                scratch: scratch.clone(),
            },
        );
        let try_dir = try_dir(&binding.attempt_dir, key, node.try_number);
        let (host_git, tree_git) = host_git(binding, &worktree)
            .map_err(|detail| fail(FailureReason::WorktreeUnavailable, detail))?;
        let program_node = ProgramNode {
            script,
            output: definition_node.output.clone(),
            permissions,
            caps: ProgramCaps::resolve(definition_node, &inputs.programs),
            polls: definition_node
                .polls
                .as_ref()
                .map(|polls| PollPlan::resolve(polls, &inputs.programs)),
            state,
            declared_dirs: Vec::new(),
            tree_access: TreeAccess::Writable,
            tree_git,
            host_git,
            worktree,
            scratch,
            node_result: try_dir.join("node-result.json"),
            try_dir,
            task_id: binding.task_id.clone(),
            task_dir: binding.task_dir.clone(),
        };
        self.runner_for(interpreter, inputs)
            .start(node.run, key, node.try_number, program_node)
    }

    fn poll(&mut self) -> Vec<Finish> {
        self.runner
            .as_mut()
            .map(ProgramRunner::poll)
            .unwrap_or_default()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        if let Some(runner) = &mut self.runner {
            runner.stop(tries);
        }
    }
}

fn todo_of(binding: &RunBinding) -> TodoRef {
    TodoRef {
        index: 0,
        name: binding
            .inputs
            .iter()
            .find(|input| input.source == "todo.name")
            .and_then(|input| input.value.as_ref())
            .map(|value| value.to_text())
            .unwrap_or_default(),
    }
}

/// What the host trusts about the node's repository, read from the run's record and never from
/// the tree, and the git the node gets in `worktree`. The common git directory comes from the
/// run worktree's recorded git directory; the worktree's own linked git directory is the one
/// under that common directory that names it ([`linked_git_dir_from_record`]). A run or branch
/// worktree gets [`TreeGit::Private`]; a worktree with no `.git` gets [`TreeGit::None`]; a `.git`
/// that is not a linked worktree of the run's repository (a directory, a pointer elsewhere)
/// refuses the start. The run's record directory is the records directory no writable root may
/// hold.
fn host_git(binding: &RunBinding, worktree: &Path) -> Result<(HostGit, TreeGit), String> {
    let records_dir = binding.run_dir.exists().then(|| binding.run_dir.clone());
    let common = common_dir_of(&binding.worktree.git_dir);
    if std::fs::symlink_metadata(worktree.join(".git")).is_err() {
        let host = HostGit {
            git_dirs: common.into_iter().collect(),
            records_dir,
        };
        return Ok((host, TreeGit::None));
    }
    let common = common?;
    let linked = linked_git_dir_from_record(worktree, &common).map_err(|error| {
        format!("the git directory of the node's worktree is not trusted: {error:#}")
    })?;
    let host = HostGit {
        git_dirs: vec![common, linked],
        records_dir,
    };
    Ok((host, TreeGit::Private))
}

/// The node's scratch directory, beside the run's worktrees and outside the record.
fn scratch_dir(binding: &RunBinding, key: &NodeKey, try_number: u32) -> PathBuf {
    binding
        .worktree
        .run_dir
        .join(crate::worktrees::SCRATCH_DIR)
        .join(&key.node)
        .join(key.branch_label())
        .join(format!("{}-{try_number}", key.pass))
}

/// The hosts of the task repository's push remote, or [`NO_ORIGIN`].
pub(crate) fn task_remote_hosts(git: &Git, binding: &RunBinding) -> Result<Vec<String>, String> {
    // The repository comes from the run's recorded git directory; no tree is asked.
    let root = repository_of(&binding.worktree.git_dir).map_err(|_| NO_ORIGIN.to_string())?;
    let url =
        futures::executor::block_on(git.run(&root, &["remote", "get-url", "--push", "origin"]))
            .map_err(|_| NO_ORIGIN.to_string())?;
    remote_hosts(&url).ok_or_else(|| NO_ORIGIN.to_string())
}

/// The host of a remote URL and its subdomain pattern: `https://github.com/o/r.git`,
/// `git@github.com:o/r.git` and `ssh://git@github.com:22/o/r.git` all give
/// `["github.com", "*.github.com"]`. `None` for a URL with no host (a local path).
pub(crate) fn remote_hosts(url: &str) -> Option<Vec<String>> {
    let url = url.trim();
    let authority = match url.split_once("://") {
        Some((scheme, rest)) => {
            if scheme.eq_ignore_ascii_case("file") {
                return None;
            }
            rest.split(['/', '?', '#']).next()?
        }
        None => {
            // scp-like: `[user@]host:path`; a path with a slash before any colon is local.
            let (head, _) = url.split_once(':')?;
            if head.contains('/') {
                return None;
            }
            head
        }
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']')?.0
    } else if url.contains("://") {
        host.split(':').next()?
    } else {
        host
    };
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(vec![host.clone(), format!("*.{host}")])
}

/// `network` with `@remote` replaced by `hosts`.
fn expand_remote(network: &[String], hosts: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in network {
        if entry == REMOTE_ENTRY {
            out.extend(hosts.iter().cloned());
        } else {
            out.push(entry.clone());
        }
    }
    out
}

#[cfg(test)]
#[path = "dispatch_program_tests.rs"]
mod tests;
