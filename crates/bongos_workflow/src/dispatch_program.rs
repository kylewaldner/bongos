//! The program node kind of the app runner: turns a node start into a [`ProgramNode`] (script of
//! the attempt's definition copy, caps, poll plan, state document, worktree, scratch, git
//! directory and network list with `@remote` resolved) and hands it to one [`ProgramRunner`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_workflow_model::record::{StartedBy, TodoRef};
use bongos_workflow_model::state_doc::{self, Paths, RunIdentity};
use bongos_workflow_model::unsandboxed::{self, AllowedBy, Mode, Permit};
use bongos_workflow_model::{Failure, FailureReason, NodeKey};

use crate::dispatch::{NodeKind, RunnerSnapshot};
use crate::dispatch_agent::node_worktree;
use crate::lifecycle::definition_copy_dir;
use crate::process::ProcessRecord;
use crate::run_view::try_dir;
use crate::runner_program::{
    Clock, Interpreter, PollPlan, ProgramCaps, ProgramNode, ProgramRunner, SandboxProbe,
};
use crate::runtime::{Finish, NodeStart, RunBinding, RunFacts};
use crate::worktrees::{Git, repository_root};

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
                todo: binding.setup.project.is_none().then(|| todo_of(binding)),
                task_id: binding.task_id.clone(),
                task_dir: binding.task_dir.clone(),
                branch: binding.checkout.branch.clone().unwrap_or_default(),
                run_branch: binding.worktree.branch.clone(),
            },
            &definition_node.with,
            Paths {
                worktree: worktree.clone(),
                scratch: scratch.clone(),
                trees: Default::default(),
            },
        );
        let try_dir = try_dir(&binding.attempt_dir, key, node.try_number);
        let snapshot = binding.setup.snapshot;
        let mode = unsandboxed::effective_mode(inputs.programs.unsandboxed, false);
        let program_node = ProgramNode {
            permit: program_permit(&binding.setup.facts, mode),
            mode,
            // Only the OS sandbox keeps a project's files folder, which holds every run's
            // records, read-only around a run's work directory.
            needs_sandbox: binding.setup.work_dir,
            script,
            output: definition_node.output.clone(),
            schema: definition_node.schema.clone(),
            permissions,
            caps: ProgramCaps::resolve(definition_node, &inputs.programs),
            polls: definition_node
                .polls
                .as_ref()
                .map(|polls| PollPlan::resolve(polls, &inputs.programs)),
            state,
            // A snapshot or a work directory is no repository of the run's: none of its git
            // metadata is writable, even when it sits inside one.
            git_common_dir: if binding.setup.branchless() {
                None
            } else {
                common_git_dir(&worktree)
            },
            worktree,
            read_only_worktree: snapshot,
            extra_writable: binding.setup.writable_dirs(),
            env: binding.setup.env_edits(),
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

/// The run's word on a program starting without the OS sandbox, from its facts and the
/// effective mode. An unattended run, a heal run and a run whose record was not read never run
/// unsandboxed (`Refuse`): nobody is there to answer, and a healer must not reach past the
/// sandbox. Any other run asks, or under `allow` is allowed by the setting.
pub(crate) fn program_permit(facts: &RunFacts, mode: Mode) -> Permit {
    match facts.started_by {
        None | Some(StartedBy::Healer) => Permit::Refuse,
        Some(_) if facts.unattended => Permit::Refuse,
        Some(_) if mode == Mode::Allow => Permit::Allowed(AllowedBy::Setting),
        Some(_) => Permit::Ask,
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

/// The common git directory of `worktree`, when it is in a git repository.
fn common_git_dir(worktree: &Path) -> Option<PathBuf> {
    let git = Git::new("git");
    let output = futures::executor::block_on(git.run(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))
    .ok()?;
    Some(PathBuf::from(output))
}

/// The hosts of the task repository's push remote, or [`NO_ORIGIN`].
pub(crate) fn task_remote_hosts(git: &Git, binding: &RunBinding) -> Result<Vec<String>, String> {
    let task_tree = binding
        .checkout
        .worktree
        .clone()
        .unwrap_or_else(|| binding.worktree.path.clone());
    let root = futures::executor::block_on(repository_root(git, &task_tree)).unwrap_or(task_tree);
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
