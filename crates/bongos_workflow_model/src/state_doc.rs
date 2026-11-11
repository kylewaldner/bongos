//! The state document a program node reads (`$BONGOS_STATE`): the run's inputs, the results of the
//! nodes that finished, the run's identity, the node's settings and its writable paths.
//!
//! It never names the run record directory: nothing in [`build`]'s arguments carries it, so a
//! script cannot learn where the run record lives from here.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::inputs::Input;
use crate::record::{RunState, TodoRef, branch_label};

/// The state document, serialized as `state.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StateDoc {
    /// Every input by name; an input without a value is `null`.
    pub inputs: BTreeMap<String, serde_json::Value>,
    pub results: BTreeMap<String, NodeResults>,
    pub run: RunInfo,
    /// The node's `with` table.
    pub settings: serde_json::Value,
    pub paths: Paths,
}

/// The results of one node: the result itself when the node ran once, else one entry per branch
/// and pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum NodeResults {
    One(serde_json::Value),
    Several(Vec<InstanceResult>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstanceResult {
    /// As [`crate::record::NodeKey::branch_label`]: `root`, or `plan-0.items-2`.
    pub branch: String,
    pub pass: u32,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunInfo {
    pub id: String,
    pub workflow: String,
    pub todo: TodoRef,
    pub task_id: String,
    pub task_dir: PathBuf,
    /// The task's branch.
    pub branch: String,
    /// The run's own branch.
    pub run_branch: String,
    /// The highest pass each node reached.
    pub passes: BTreeMap<String, u32>,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Paths {
    pub worktree: PathBuf,
    pub scratch: PathBuf,
}

/// What [`build`] needs about the run besides its state.
#[derive(Debug, Clone, PartialEq)]
pub struct RunIdentity {
    pub id: String,
    pub workflow: String,
    pub todo: TodoRef,
    pub task_id: String,
    pub task_dir: PathBuf,
    pub branch: String,
    pub run_branch: String,
}

/// Builds the state document of one program node try. Results are the last successful try of each
/// node instance in `state`; `passes` and `cost_usd` come from `state` too.
pub fn build(
    inputs: &[Input],
    state: &RunState,
    run: RunIdentity,
    settings: &toml::Table,
    paths: Paths,
) -> StateDoc {
    let inputs = inputs
        .iter()
        .map(|input| {
            let value = input
                .value
                .as_ref()
                .map_or(serde_json::Value::Null, |value| {
                    serde_json::to_value(value).expect("input values always serialize")
                });
            (input.name.clone(), value)
        })
        .collect();

    // The last successful finish of each instance wins (a retry replaces an earlier result).
    let mut by_key = BTreeMap::new();
    for finished in &state.finished {
        if let (Some(result), None) = (&finished.result, &finished.failure) {
            by_key.insert(finished.key.clone(), result.clone());
        }
    }
    let mut by_node: BTreeMap<String, Vec<InstanceResult>> = BTreeMap::new();
    for (key, result) in by_key {
        by_node
            .entry(key.node.clone())
            .or_default()
            .push(InstanceResult {
                branch: branch_label(&key.branch),
                pass: key.pass,
                result,
            });
    }
    let results = by_node
        .into_iter()
        .map(|(node, mut instances)| {
            let results = if instances.len() == 1 {
                NodeResults::One(instances.remove(0).result)
            } else {
                NodeResults::Several(instances)
            };
            (node, results)
        })
        .collect();

    StateDoc {
        inputs,
        results,
        run: RunInfo {
            id: run.id,
            workflow: run.workflow,
            todo: run.todo,
            task_id: run.task_id,
            task_dir: run.task_dir,
            branch: run.branch,
            run_branch: run.run_branch,
            passes: state.passes.clone(),
            cost_usd: state.cost_usd,
        },
        settings: serde_json::to_value(settings).expect("toml tables always serialize"),
        paths,
    }
}

impl StateDoc {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("the state document always serializes")
    }

    /// Writes the document to `path` read-only (mode 0444 on Unix), replacing a file already
    /// there.
    pub fn write_readonly(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::write(path, self.to_json())?;
        let mut permissions = std::fs::metadata(path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o444);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions)
    }
}
