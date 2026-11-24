//! The two definition-side switches of the workflow sandbox: `runs.unsandboxed` in `project.toml`,
//! which can only tighten the user's `workflows.programs.unsandboxed`, and `with.env` of any node of
//! a workflow definition, which names the writer's variables a `wait` receives.
//!
//! The approval hook `workflow-sandbox-switch` holds for a person, whoever wrote it, every change
//! that adds, removes or alters either switch, and every change that leaves a file holding one
//! unreadable. A change that leaves both alone gets no opinion. It reads only the two trees.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use bongos_project_repo::{ApprovalHook, ApprovalHooks, ApprovalInput, ApprovalOpinion};
use bongos_project_repo_model::layout::{
    DEFINITIONS_DIR, PROJECT_FILE, WORKFLOW_FILE, WORKFLOWS_DIR,
};
use gpui::App;

use crate::lifecycle::RunId;

pub(crate) const HOOK_NAME: &str = "workflow-sandbox-switch";

/// The project key that refuses unsandboxed starts.
pub(crate) const RUNS_UNSANDBOXED: &str = "runs.unsandboxed";

/// The only value `runs.unsandboxed` has.
const REFUSE: &str = "refuse";

pub(crate) fn register(cx: &mut App) {
    ApprovalHooks::register(Arc::new(SandboxSwitch), cx);
}

/// What [`refusing_runs`] read.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Refusing {
    /// The runs whose project refuses unsandboxed starts.
    pub(crate) runs: BTreeSet<RunId>,
    /// The runs whose value is not `"refuse"` and is read as it.
    pub(crate) misread: BTreeSet<RunId>,
    /// The warnings to log.
    pub(crate) warnings: Vec<String>,
}

/// The runs whose project refuses unsandboxed starts, from each run's `runs.unsandboxed`. No value
/// sets no override and `"refuse"` refuses. Any other value refuses too (fail closed), with a
/// warning when its run was not misread already (`misread_before`, the last read's `misread`).
pub(crate) fn refusing_runs(
    values: impl IntoIterator<Item = (RunId, Option<toml::Value>)>,
    misread_before: &BTreeSet<RunId>,
) -> Refusing {
    let mut refusing = Refusing::default();
    for (run, value) in values {
        match value {
            None => continue,
            Some(toml::Value::String(text)) if text == REFUSE => {}
            Some(other) => {
                if !misread_before.contains(&run) {
                    refusing.warnings.push(format!(
                        "run {run}: {RUNS_UNSANDBOXED} = {other} is not \"{REFUSE}\"; it is read as \"{REFUSE}\""
                    ));
                }
                refusing.misread.insert(run.clone());
            }
        }
        refusing.runs.insert(run);
    }
    refusing
}

/// The `workflow-sandbox-switch` approval hook.
pub(crate) struct SandboxSwitch;

impl ApprovalHook for SandboxSwitch {
    fn name(&self) -> &str {
        HOOK_NAME
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion {
        let master = input.master_root.join(DEFINITIONS_DIR);
        let change = input.change_root.join(DEFINITIONS_DIR);
        let mut found = Vec::new();
        match tables(&master, &change, Path::new(PROJECT_FILE)) {
            None => {}
            Some(Err(())) => found.push(format!("cannot read {RUNS_UNSANDBOXED}")),
            Some(Ok((old, new))) => {
                if runs_unsandboxed(&old) != runs_unsandboxed(&new) {
                    found.push(switch_text(RUNS_UNSANDBOXED));
                }
            }
        }
        let names: BTreeSet<OsString> = workflow_names(&master)
            .into_iter()
            .chain(workflow_names(&change))
            .collect();
        for name in names {
            let relative = Path::new(WORKFLOWS_DIR).join(&name).join(WORKFLOW_FILE);
            let file = format!("{WORKFLOWS_DIR}/{}/{WORKFLOW_FILE}", name.to_string_lossy());
            let (old, new) = match tables(&master, &change, &relative) {
                None => continue,
                Some(Err(())) => {
                    found.push(format!("cannot read nodes.*.with.env in {file}"));
                    continue;
                }
                Some(Ok(tables)) => tables,
            };
            let (old, new) = (node_envs(&old), node_envs(&new));
            let ids: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
            for id in ids {
                if old.get(id) != new.get(id) {
                    let key = format!("nodes.{id}.with.env");
                    found.push(format!("{} in {file}", switch_text(&key)));
                }
            }
        }
        if found.is_empty() {
            ApprovalOpinion::NoOpinion
        } else {
            ApprovalOpinion::NeedsPerson(found.join("; "))
        }
    }
}

fn switch_text(key: &str) -> String {
    format!("changes the workflow sandbox switch {key}")
}

/// The file at `relative` in master's and the change's definitions, parsed: `None` when its bytes
/// are the same on both sides, `Err` when either side cannot be read or is not TOML in UTF-8. A
/// missing file reads as an empty table.
fn tables(
    master: &Path,
    change: &Path,
    relative: &Path,
) -> Option<Result<(toml::Table, toml::Table), ()>> {
    let (old, new) = match (bytes(&master.join(relative)), bytes(&change.join(relative))) {
        (Ok(old), Ok(new)) => (old, new),
        _ => return Some(Err(())),
    };
    if old == new {
        return None;
    }
    Some(parse(old).and_then(|old| Ok((old, parse(new)?))))
}

/// The bytes of the file at `path`, `None` when there is none.
fn bytes(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// The table in `bytes`, empty for no file.
fn parse(bytes: Option<Vec<u8>>) -> Result<toml::Table, ()> {
    let Some(bytes) = bytes else {
        return Ok(toml::Table::new());
    };
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    text.parse::<toml::Table>().map_err(|_| ())
}

fn runs_unsandboxed(project: &toml::Table) -> Option<&toml::Value> {
    project.get("runs")?.get("unsandboxed")
}

/// Each node's `with.env`, by node id.
fn node_envs(definition: &toml::Table) -> BTreeMap<String, toml::Value> {
    definition
        .get("nodes")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter_map(|(id, node)| Some((id.clone(), node.get("with")?.get("env")?.clone())))
        .collect()
}

/// The workflow definition folders under `definitions/workflows`, whatever their names' encoding.
fn workflow_names(definitions: &Path) -> Vec<OsString> {
    let Ok(entries) = fs::read_dir(definitions.join(WORKFLOWS_DIR)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name())
        .collect()
}

#[cfg(test)]
#[path = "sandbox_switch_tests.rs"]
pub(crate) mod tests;
