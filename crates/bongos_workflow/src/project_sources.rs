//! What workflow runs tell a project repository: the live source that makes a task
//! with an active run started for the landing checks, the activity source a restore waits on, and
//! the rows of the `runs` namespace of the state store that a snapshot writes as `runs.toml`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context as _, Result};
use bongos_project_repo::{ActivitySource, LiveSource, ProjectRepo, RUNS_NAMESPACE};

use crate::engine::SharedLifecycle;
use crate::lifecycle::{RunId, RunStatus};

/// The name of the live source and the prefix of the lines of the activity source.
pub const SOURCE_NAME: &str = "workflow runs";

/// The project repository of each run started in a new-format project.
pub(crate) type RunProjects = Arc<Mutex<BTreeMap<RunId, ProjectRepo>>>;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The projects a restore holds new work back in. Shared by the engine, which refuses to start
/// work in a held project, and the activity source, which takes and ends the hold.
#[derive(Clone, Default)]
pub struct WorkHolds(Arc<Mutex<BTreeMap<String, usize>>>);

impl WorkHolds {
    pub fn is_held(&self, project_id: &str) -> bool {
        locked(&self.0).contains_key(project_id)
    }

    fn hold(&self, project_id: &str) {
        *locked(&self.0).entry(project_id.to_string()).or_default() += 1;
    }

    fn release(&self, project_id: &str) {
        let mut holds = locked(&self.0);
        if let Some(count) = holds.get_mut(project_id) {
            *count -= 1;
            if *count == 0 {
                holds.remove(project_id);
            }
        }
    }
}

/// Makes a task with an active run started, so a landing never changes the definition of work
/// that is running.
pub(crate) struct WorkflowLiveSource {
    pub(crate) runs: SharedLifecycle,
}

impl LiveSource for WorkflowLiveSource {
    fn name(&self) -> &str {
        SOURCE_NAME
    }

    fn started(&self, task_id: &str) -> bool {
        locked(&self.runs).active_run_id(task_id).is_some()
    }
}

/// Reports the runs executing in a project, stops them at their next node boundary and holds
/// back the work that would start.
pub(crate) struct WorkflowActivity {
    pub(crate) runs: SharedLifecycle,
    pub(crate) projects: RunProjects,
    pub(crate) holds: WorkHolds,
}

impl WorkflowActivity {
    /// The runs of `project_id` with work in flight: queued or running (a paused run, a halted
    /// one, one waiting on a person and a failed one execute nothing).
    fn executing(&self, project_id: &str) -> Vec<(RunId, String, RunStatus)> {
        let in_project: Vec<RunId> = locked(&self.projects)
            .iter()
            .filter(|(_, repo)| repo.location().id == project_id)
            .map(|(run, _)| run.clone())
            .collect();
        let lifecycle = locked(&self.runs);
        in_project
            .into_iter()
            .filter_map(|run| {
                let status = lifecycle.status(&run)?;
                let workflow = lifecycle.record(&run)?.workflow.clone();
                matches!(
                    status,
                    RunStatus::Queued | RunStatus::Running | RunStatus::Pausing
                )
                .then_some((run, workflow, status))
            })
            .collect()
    }
}

impl ActivitySource for WorkflowActivity {
    fn name(&self) -> &str {
        SOURCE_NAME
    }

    fn activity(&self, project_id: &str) -> Vec<String> {
        self.executing(project_id)
            .into_iter()
            .map(|(run, workflow, status)| format!("{workflow} {}, run {run}", status.label()))
            .collect()
    }

    /// Pauses each executing run: no node starts, the running ones finish, then it reads Paused
    /// and no longer counts as activity.
    fn stop_at_job_boundary(&self, project_id: &str) -> Result<()> {
        let mut failed = Vec::new();
        for (run, _, status) in self.executing(project_id) {
            if status == RunStatus::Pausing {
                continue;
            }
            if let Err(refusal) = locked(&self.runs).pause(&run) {
                failed.push(format!("run {run}: {refusal}"));
            }
        }
        anyhow::ensure!(failed.is_empty(), "{}", failed.join("; "));
        Ok(())
    }

    fn hold_new_work(&self, project_id: &str) {
        self.holds.hold(project_id);
    }

    fn release_new_work(&self, project_id: &str) {
        self.holds.release(project_id);
    }
}

/// The row of `runs.toml` for a run: its task, workflow and state, and for a run that ended
/// how, as the snapshot of the state store exports it.
pub(crate) fn run_row(task_id: &str, workflow: &str, status: RunStatus) -> toml::Table {
    let mut row = toml::Table::new();
    row.insert("task".to_string(), task_id.to_string().into());
    row.insert("workflow".to_string(), workflow.to_string().into());
    row.insert("state".to_string(), status.label().to_string().into());
    let outcome = match status {
        RunStatus::Succeeded => Some("passed"),
        RunStatus::Failed => Some("failed"),
        RunStatus::Cancelled => Some("cancelled"),
        _ => None,
    };
    if let Some(outcome) = outcome {
        row.insert("outcome".to_string(), outcome.to_string().into());
    }
    row
}

/// Writes the row of `run` into the `runs` namespace of the project's state store. A window that
/// only reads the store (another process holds the writer) writes nothing.
pub(crate) fn export_run(repo: &ProjectRepo, run: &str, row: toml::Table) -> Result<()> {
    let Some(writer) = repo.state().and_then(|state| state.writer()) else {
        return Ok(());
    };
    writer
        .transact(|tx| tx.kv(RUNS_NAMESPACE).put(run, toml::Value::Table(row)))
        .with_context(|| format!("recording run {run} in the project's state"))
}
