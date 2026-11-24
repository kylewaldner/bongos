//! The agent node kind of the app runner: turns a node start into a [`ClaudeNode`] (caps, rendered
//! prompt, system text, try directory, branch worktree) and hands it to one [`ClaudeRunner`].

use std::fs;

use bongos_work_model::artifacts::{project_session_context, session_context};
use bongos_work_model::new_id;
use bongos_workflow_model::record::TodoRef;
use bongos_workflow_model::template::{Scope, render};
use bongos_workflow_model::{Failure, FailureReason, NodeKey, RunState};

use crate::dispatch::{NodeKind, RunnerSnapshot};
use crate::lifecycle::{definition_copy_dir, prompt_with_revision, schema_text};
use crate::process::ProcessRecord;
use crate::run_view::try_dir;
use crate::runner_claude::{ClaudeCli, ClaudeNode, ClaudeRunner, NodeCaps};
use crate::runtime::{Finish, NodeStart, RunBinding};
use crate::task_link::{project_system_text, prompt_scope, system_text};
use crate::worktrees::{Git, branch_worktree_path, create_branch};
use bongos_workflow_model::definition::AgentKind;

/// Runs Claude agent nodes through one [`ClaudeRunner`], built from the snapshot's CLI check.
#[derive(Default)]
pub(crate) struct AgentNodes {
    runner: Option<ClaudeRunner>,
}

impl AgentNodes {
    /// The runner for `cli`; a runner with tries in flight is kept, since dropping it would lose
    /// them, and replaced once it is idle and the CLI changed.
    fn runner_for(&mut self, cli: ClaudeCli, snapshot: &RunnerSnapshot) -> &mut ClaudeRunner {
        let stale = self
            .runner
            .as_ref()
            .is_none_or(|runner| runner.cli() != &cli && runner.in_flight().is_empty());
        if stale {
            self.runner = Some(ClaudeRunner::new(cli, snapshot.base_env.clone()));
        }
        self.runner.get_or_insert_with(|| unreachable!())
    }
}

impl NodeKind for AgentNodes {
    fn start(
        &mut self,
        node: &NodeStart<'_>,
        binding: &RunBinding,
        inputs: &RunnerSnapshot,
    ) -> Result<ProcessRecord, Failure> {
        let key = node.key;
        let fail = |reason, detail: String| Failure::new(reason, Some(key.clone()), detail);
        let cli = match &inputs.claude {
            Some(Ok(cli)) => cli.clone(),
            Some(Err(failure)) => {
                return Err(Failure::new(
                    failure.reason,
                    Some(key.clone()),
                    failure.detail.clone(),
                ));
            }
            None => {
                return Err(fail(
                    FailureReason::AgentNotInstalledOrLoggedOut,
                    "Claude Code was not checked yet".to_string(),
                ));
            }
        };
        let definition_node = node.definition.node(&key.node).ok_or_else(|| {
            fail(
                FailureReason::NodeOutputInvalid,
                format!("the definition has no node '{}'", key.node),
            )
        })?;
        let mut caps = NodeCaps::resolve(node.definition, definition_node, &inputs.agents);
        if let Some(left) = node.budget_left_usd {
            caps.max_cost_usd = caps.max_cost_usd.min(left.max(0.0));
        }
        let prompt = render_node_prompt(node, binding)
            .map_err(|detail| fail(FailureReason::NodeOutputInvalid, detail))?;
        let system = system_for(binding)
            .map_err(|detail| fail(FailureReason::WorktreeUnavailable, detail))?;
        let worktree = node_worktree(key, binding)
            .map_err(|detail| fail(FailureReason::WorktreeUnavailable, detail))?;
        let schema_text = schema_text(
            &binding.attempt_dir,
            node.state.definition_rev.unwrap_or(0),
            definition_node,
        )
        .map_err(|detail| fail(FailureReason::WorktreeUnavailable, detail))?;
        let try_dir = try_dir(&binding.attempt_dir, key, node.try_number);
        let claude_node = ClaudeNode {
            caps,
            output: definition_node.output.clone(),
            schema: definition_node.schema.clone(),
            schema_text,
            mcp_exclude: inputs.mcp_exclude.clone(),
            mcp_installed: inputs
                .mcp_installed
                .get(&AgentKind::Claude)
                .cloned()
                .unwrap_or_default(),
            prompt,
            system,
            worktree,
            node_result: try_dir.join("node-result.json"),
            try_dir,
            task_id: binding.task_id.clone(),
            task_dir: binding.task_dir.clone(),
            session_id: new_id(),
            env: binding.setup.env_edits(),
        };
        self.runner_for(cli, inputs)
            .start(node.run, key, node.try_number, claude_node)
    }

    fn poll(&mut self) -> Vec<Finish> {
        self.runner
            .as_mut()
            .map(ClaudeRunner::poll)
            .unwrap_or_default()
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        if let Some(runner) = &mut self.runner {
            runner.stop(tries);
        }
    }
}

/// The node's prompt file rendered over the run's inputs and the results of finished nodes, with
/// the revision comment of its last gate appended.
fn render_node_prompt(node: &NodeStart<'_>, binding: &RunBinding) -> Result<String, String> {
    let definition_node = node
        .definition
        .node(&node.key.node)
        .ok_or_else(|| format!("the definition has no node '{}'", node.key.node))?;
    let file = definition_node
        .prompt
        .as_ref()
        .ok_or_else(|| format!("node '{}' has no prompt file", node.key.node))?;
    let dir = definition_copy_dir(&binding.attempt_dir, node.state.definition_rev.unwrap_or(0));
    let path = dir.join(&file.value);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut scope = prompt_scope(&binding.inputs);
    add_results(&mut scope, node.state);
    add_item(&mut scope, node);
    let base = render(&text, &scope).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(prompt_with_revision(node.state, node.key, &base))
}

/// `nodes.<node>.<field>` from the last successful result of each node.
fn add_results(scope: &mut Scope, state: &RunState) {
    for finished in &state.finished {
        let Some(result) = finished.result.as_ref().filter(|_| finished.succeeded()) else {
            continue;
        };
        let Some(fields) = result.as_object() else {
            continue;
        };
        for (field, value) in fields {
            let text = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            scope.set(format!("nodes.{}.{field}", finished.key.node), text);
        }
    }
}

/// `item`: for a key inside a fan-out branch, the item of its innermost fan, read from the list
/// the fan node's last successful result fans out over.
fn add_item(scope: &mut Scope, node: &NodeStart<'_>) {
    let Some(((fan, index), outer)) = node.key.branch.split_last() else {
        return;
    };
    let Some(result) = node
        .state
        .finished
        .iter()
        .rev()
        .filter(|finished| finished.key.node == *fan && finished.key.branch == outer)
        .find_map(|finished| finished.result.as_ref().filter(|_| finished.succeeded()))
    else {
        return;
    };
    let Some(fan_node) = node.definition.node(fan) else {
        return;
    };
    let item = fan_node
        .routes
        .iter()
        .filter_map(|route| route.fan_out.as_ref())
        .find_map(|fan_out| match result.get(&fan_out.over) {
            Some(serde_json::Value::Array(items)) => items.get(*index as usize),
            _ => None,
        });
    if let Some(item) = item {
        let text = match item {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        scope.set("item", text);
    }
}

/// The system text: the project context of `project_session_context` for a run with no task,
/// else the task context of `session_context`.
pub(crate) fn system_for(binding: &RunBinding) -> Result<String, String> {
    if let Some(source) = &binding.setup.project {
        let context =
            project_session_context(&source.project, &source.files_dir, &source.definitions)
                .map_err(|error| format!("{}: {error:#}", source.files_dir.display()))?;
        return Ok(project_system_text(&context, &binding.workflow));
    }
    let context = session_context(&binding.task_dir)
        .map_err(|error| format!("{}: {error:#}", binding.task_dir.display()))?;
    let todo = TodoRef {
        index: 0,
        name: binding
            .inputs
            .iter()
            .find(|input| input.source == "todo.name")
            .and_then(|input| input.value.as_ref())
            .map(|value| value.to_text())
            .unwrap_or_default(),
    };
    Ok(system_text(&context, &binding.workflow, &todo))
}

/// Where `key` runs: the run worktree, or for a key inside a fan-out its branch worktree, which
/// is created from the run branch when it does not exist yet. A run on a snapshot or in a
/// project's work directory has no branch to make one from: every key runs in its one tree.
pub(crate) fn node_worktree(
    key: &NodeKey,
    binding: &RunBinding,
) -> Result<std::path::PathBuf, String> {
    let Some((fan, index)) = key.branch.last().filter(|_| !binding.setup.branchless()) else {
        return Ok(binding.worktree.path.clone());
    };
    let path = branch_worktree_path(&binding.worktree.run_dir, fan, *index);
    if !path.exists() {
        futures::executor::block_on(create_branch(
            &Git::new("git"),
            &binding.worktree,
            fan,
            *index,
        ))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
#[path = "dispatch_agent_tests.rs"]
mod tests;
