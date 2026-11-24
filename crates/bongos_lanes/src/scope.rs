//! The `lane-run-scope` approval hook, one of the `declared-scope` rule's hooks: a change by a
//! workflow run that a lane runs as its job is approved when everything it touches is inside the
//! `documents/` or `tasks/` folder of the top-level task its lane owns. The lane and its task come
//! from the lanes state ([`LaneRuns::owned_scope_in`]), never from the change. An edit of the
//! owned task's own definition, anything outside the scope, and any change by a run no lane holds
//! (one started by hand) get no opinion here, so they wait for a person.

use bongos_project_repo::{
    ApprovalHook, ApprovalHooks, ApprovalInput, ApprovalOpinion, AutomaticRule,
};
use bongos_project_repo_model::AuthorKind;
use bongos_project_repo_model::layout::{DOCUMENTS_DIR, TASKS_DIR};
use gpui::App;

use crate::runs::LaneRuns;

/// Judges lane runs' changes against their owned scope.
pub struct LaneRunScope {
    runs: LaneRuns,
}

impl LaneRunScope {
    pub const NAME: &'static str = "lane-run-scope";

    pub fn new(runs: LaneRuns) -> Self {
        Self { runs }
    }

    /// Registers the hook over `runs`.
    pub fn register(runs: LaneRuns, cx: &mut App) {
        ApprovalHooks::register(std::sync::Arc::new(Self::new(runs)), cx);
    }
}

/// `folder` with a trailing slash, or nothing for the top group's folder.
fn prefix(folder: &str) -> String {
    if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    }
}

/// Whether the file `path` (relative to `definitions/`) is inside the `documents/` or `tasks/`
/// folder of the task whose folder is `folder`.
fn in_owned_scope(folder: &str, path: &str) -> bool {
    if path.split('/').any(|part| part == "..") {
        return false;
    }
    let Some(rest) = path.strip_prefix(&prefix(folder)) else {
        return false;
    };
    rest.starts_with(&format!("{DOCUMENTS_DIR}/")) || rest.starts_with(&format!("{TASKS_DIR}/"))
}

/// Whether a task whose folder is `task_folder` is strictly below the task whose folder is
/// `folder`, in its `tasks/` folder.
fn below(folder: &str, task_folder: &str) -> bool {
    task_folder.starts_with(&format!("{}{TASKS_DIR}/", prefix(folder)))
}

/// Whether every task and file `input`'s change touches lies under the `documents/` or `tasks/`
/// folder of the top-level task `task`, in master and in the change. The task's own `task.toml`
/// (its goal, edges, size, checks, work kind and areas) is outside.
fn inside_owned_task(input: &ApprovalInput, task: &str) -> bool {
    let Some(root) = input.master.node(task) else {
        return false;
    };
    let folder = root.folder.as_str();
    let summary = input.summary;
    let touched = summary
        .tasks_added
        .iter()
        .chain(&summary.tasks_removed)
        .chain(&summary.tasks_redefined)
        .chain(&summary.reparented);
    for id in touched {
        if id == task {
            return false;
        }
        for tree in [input.master, input.change] {
            if let Some(node) = tree.node(id)
                && !below(folder, &node.folder)
            {
                return false;
            }
        }
    }
    summary
        .files
        .iter()
        .all(|(path, _)| in_owned_scope(folder, path))
}

impl ApprovalHook for LaneRunScope {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn rule(&self) -> Option<AutomaticRule> {
        Some(AutomaticRule::DeclaredScope)
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion {
        let author = &input.meta.author;
        if author.kind() != AuthorKind::Workflow {
            return ApprovalOpinion::NoOpinion;
        }
        let Some(run) = author.run.as_deref() else {
            return ApprovalOpinion::NoOpinion;
        };
        let Some(scope) = self.runs.owned_scope_in(input.project_id, run) else {
            return ApprovalOpinion::NoOpinion;
        };
        let (lane, task) = (&scope.lane, scope.task.0.as_str());
        if !inside_owned_task(input, task) {
            return ApprovalOpinion::NoOpinion;
        }
        ApprovalOpinion::Approve(format!(
            "lane {lane} run {run} changed only the documents and tasks of its task {task}"
        ))
    }
}
